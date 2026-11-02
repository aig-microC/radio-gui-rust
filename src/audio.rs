/*
 * =================================================================================
 * PROYECTO: Radio por internet en RUST
 * =================================================================================
 *
 *  Autor: Angel de la Iglesia Gonzalo
 *  Version/rev: 1/0
 *  Licencia: MIT - Ver LICENSE
 *  Fecha: 20261102
 *
 * =================================================================================
 */
//! Motor de reproducción de radio por Internet en Rust.
//!
//! Sustituye a los reproductores externos por un reproductor nativo basado en
//! `rodio` (salida de audio) y `symphonia` (demuxado y decodificación de MP3
//! **y AAC** en flujos elementales).
//!
//! Características principales:
//! - Soporta emisoras **MP3** y **AAC/AAC+** (flujo elemental ADTS). Los flujos
//!   ADTS se normalizan antes de decodificar (encuadre de tramas y conversión
//!   MPEG-2 `0xFFF9` → MPEG-4 `0xFFF1`), porque `symphonia` solo reconoce la
//!   sincronía MPEG-4; de otro modo las emisoras `audio/aacp`/HE-AAC de Icecast
//!   no sonarían.
//! - Resuelve **playlists** (`.m3u` / `.pls`) extrayendo la URL real.
//! - Reproduce **HLS** (`.m3u8`): descarga la lista de segmentos y extrae el
//!   flujo de audio elemental, ya venga dentro de **MPEG-TS** (demultiplexado)
//!   o como **flujo elemental directo** (p. ej. ADTS AAC, quitando su etiqueta
//!   ID3). Lo decodifica de forma continua recargando la playlist para live. La
//!   posición se sigue por número de secuencia (`#EXT-X-MEDIA-SEQUENCE`) y
//!   avanza siempre hacia delante, de modo que un retraso frente al directo
//!   produce saltos pero nunca fragmentos repetidos.
//! - La decodificación corre en un hilo de fondo que rellena una **cola de PCM**
//!   (`AudioQueue`); rodio consume de esa cola sin bloquear nunca el hilo de
//!   audio. La cola (~3 s) absorbe las esperas de red y las fronteras entre
//!   segmentos, con lo que el dispositivo deja de sufrir `underrun` de ALSA.
//! - Reconexión automática, timeouts y re-sincronización ante caídas, igual
//!   que el reproductor de línea de comandos original.
//!
//! El cambio de emisora "al vuelo" se hace marcando una bandera `alive`
//! compartida que agota el iterador de la fuente; entonces se inyecta la nueva
//! fuente en el mismo `OutputStreamHandle` de rodio.

use std::cmp::min;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT};
use rodio::{OutputStream, OutputStreamHandle, Sink, Source};
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SError;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::probe::{Hint, ProbeResult};
use symphonia::default::{get_codecs, get_probe};

/// Registro con marca de tiempo `HH:MM:SS.mmm` para identificar cortes y sus
/// duraciones en la salida por terminal.
macro_rules! log {
    ($tag:expr, $($arg:tt)*) => {
        eprintln!(
            "[{}] [{}] {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            $tag,
            format!($($arg)*)
        )
    };
}

const READ_TIMEOUT: Duration = Duration::from_secs(2);
const CHUNK_SIZE: usize = 64 * 1024;

/// Número máximo de intentos de conexión con una emisora que aún no ha conectado
/// nunca. Evita quedarse en «Conectando…» indefinidamente con una URL caída: al
/// agotarse se informa del error y la interfaz deja de insistir. Si la emisora ya
/// sonó alguna vez, los reintentos no tienen límite (puede recuperarse).
const MAX_INTENTOS_CONEXION: u32 = 3;

/// Ejecuta `f` capturando cualquier pánico. Devuelve `Some(valor)` si terminó
/// con normalidad y `None` si paniqueó (registrando el motivo). Se usa en los
/// hilos de trabajo (red/decodificación) para que un fallo imprevisto (datos
/// corruptos, etc.) no termine todo el proceso: el hilo finaliza y la interfaz
/// sigue viva y operativa.
fn capturar<T>(etiqueta: &'static str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(panico) => {
            let msg = panico
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panico.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "motivo desconocido".to_string());
            log!("panic", "hilo '{etiqueta}': {msg}");
            None
        }
    }
}

/// Variante de `capturar` para funciones sin valor de retorno: devuelve `true`
/// si se produjo un pánico.
fn ejecutar_protegido(etiqueta: &'static str, f: impl FnOnce()) -> bool {
    capturar(etiqueta, f).is_none()
}

/// Acceso a `Mutex` que tolera el "envenenamiento": si otro hilo paniqueó
/// mientras tenía el candado, seguimos usando el dato en lugar de propagar el
/// pánico (que tumbaría la interfaz gráfica).
trait CandadoExt<T> {
    fn candado(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> CandadoExt<T> for Mutex<T> {
    fn candado(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Cabeceras HTTP comunes para que los servidores de radio acepten la petición.
fn build_client() -> io::Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Mozilla/5.0 (rust-radio-client)"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

    Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .default_headers(headers)
        .build()
        .map_err(io::Error::other)
}

/// Lector HTTP que se reconecta de forma transparente ante caídas de red,
/// fin de flujo (EOF) o errores/congestiones de lectura. Para el consumidor
/// (el decodificador) parece un flujo infinito e ininterrumpido.
///
/// `reqwest::blocking` no expone `read_timeout`, así que implementamos nuestro
/// propio timeout de lectura: cada lectura se hace en un hilo aparte y, si no
/// responde en `READ_TIMEOUT`, se abandona la conexión y se reconecta.
struct ReconnectingReader {
    url: String,
    client: Client,
    inner: Option<Box<dyn Read + Send + Sync>>,
    stash: Vec<u8>,
    stash_pos: usize,
    alive: Arc<AtomicBool>,
    /// Indica si la emisora llegó a conectar con éxito alguna vez.
    conectado: bool,
    /// Motivo del fallo definitivo de conexión inicial. Una vez fijado, las
    /// lecturas devuelven este error de inmediato (evita reintentos en bucle).
    fallo: Option<String>,
}

impl ReconnectingReader {
    fn new(url: String, alive: Arc<AtomicBool>, client: Client) -> Self {
        Self {
            url,
            client,
            inner: None,
            stash: Vec::new(),
            stash_pos: 0,
            alive,
            conectado: false,
            fallo: None,
        }
    }

    /// Intenta conectar. Devuelve error solo cuando se rinde de forma definitiva:
    /// bien porque el servidor responde un error de cliente (4xx) antes de haber
    /// conectado nunca, bien porque se agotan los `MAX_INTENTOS_CONEXION`. Si la
    /// emisora ya había conectado, reintenta sin límite (puede recuperarse).
    fn connect(&mut self) -> io::Result<()> {
        let mut delay = Duration::from_millis(500);
        let mut intentos = 0u32;
        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return Ok(());
            }
            match self.client.get(&self.url).send() {
                Ok(resp) if resp.status().is_success() => {
                    self.inner = Some(Box::new(resp));
                    self.conectado = true;
                    log!("net", "conectado a {}", self.url);
                    return Ok(());
                }
                Ok(resp) => {
                    let st = resp.status();
                    log!("net", "respuesta inesperada: {st}");
                    if st.is_client_error() && !self.conectado {
                        // 403/404…: la URL no existe o está bloqueada. No insistir.
                        let msg = format!("la emisora respondió {st}");
                        self.fallo = Some(msg.clone());
                        return Err(io::Error::other(msg));
                    }
                }
                Err(e) => log!("net", "fallo de conexión: {e}"),
            }
            intentos += 1;
            if !self.conectado && intentos >= MAX_INTENTOS_CONEXION {
                let msg = format!(
                    "no se pudo conectar con la emisora tras {MAX_INTENTOS_CONEXION} intentos"
                );
                self.fallo = Some(msg.clone());
                return Err(io::Error::new(io::ErrorKind::TimedOut, msg));
            }
            if !self.alive.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    fn timed_read(&mut self) -> io::Result<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        // Defensivo: si no hay conexión se trata como EOF en lugar de entrar en
        // pánico (el llamante reconecta).
        let mut inner = match self.inner.take() {
            Some(inner) => inner,
            None => return Ok(Vec::new()),
        };
        std::thread::spawn(move || {
            let mut tmp = vec![0u8; CHUNK_SIZE];
            match inner.read(&mut tmp) {
                Ok(n) => {
                    tmp.truncate(n);
                    let _ = tx.send((Ok(tmp), inner));
                }
                Err(e) => {
                    let _ = tx.send((Err(e), inner));
                }
            }
        });

        match rx.recv_timeout(READ_TIMEOUT) {
            Ok((Ok(data), inner)) => {
                self.inner = Some(inner);
                Ok(data)
            }
            Ok((Err(e), inner)) => {
                self.inner = Some(inner);
                Err(e)
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "lectura sin respuesta, reconectando",
            )),
        }
    }
}

impl Read for ReconnectingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.stash_pos < self.stash.len() {
            let n = min(buf.len(), self.stash.len() - self.stash_pos);
            buf[..n].copy_from_slice(&self.stash[self.stash_pos..self.stash_pos + n]);
            self.stash_pos += n;
            if self.stash_pos >= self.stash.len() {
                self.stash.clear();
                self.stash_pos = 0;
            }
            return Ok(n);
        }

        // Fallo de conexión ya considerado definitivo: se informa siempre igual.
        if let Some(msg) = &self.fallo {
            return Err(io::Error::other(msg.clone()));
        }

        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return Ok(0);
            }
            if self.inner.is_none() {
                self.connect()?;
                if self.inner.is_none() {
                    return Ok(0);
                }
            }
            match self.timed_read() {
                Ok(chunk) => {
                    if chunk.is_empty() {
                        log!("net", "fin de flujo (EOF), reconectando...");
                        self.inner = None;
                        continue;
                    }
                    let total = chunk.len();
                    let m = min(buf.len(), total);
                    buf[..m].copy_from_slice(&chunk[..m]);
                    if m < total {
                        self.stash = chunk;
                        self.stash_pos = m;
                    } else {
                        self.stash.clear();
                        self.stash_pos = 0;
                    }
                    return Ok(m);
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log!("net", "error de lectura: {e}, reconectando...");
                    self.inner = None;
                    self.stash.clear();
                    self.stash_pos = 0;
                    continue;
                }
            }
        }
    }
}

/// Demuxor MPEG-TS mínimo: extrae el flujo elemental de audio (ADTS AAC o MP3)
/// de los paquetes TS de un segmento HLS y lo concatena en `out`.
struct TsState {
    pmt_pid: Option<u16>,
    audio_pid: Option<u16>,
}

impl TsState {
    fn new() -> Self {
        Self {
            pmt_pid: None,
            audio_pid: None,
        }
    }
}

fn parse_pat(p: &[u8], st: &mut TsState) {
    if p.is_empty() {
        return;
    }
    let start = 1 + p[0] as usize;
    if start + 8 > p.len() {
        return;
    }
    if p[start] != 0x00 {
        return;
    }
    let sec_len = (((p[start + 1] & 0x0F) as usize) << 8) | p[start + 2] as usize;
    let end = start + 3 + sec_len;
    let mut j = start + 8;
    while j + 4 <= end && j + 4 <= p.len() {
        let prog = u16::from_be_bytes([p[j], p[j + 1]]);
        let pmt_pid = ((p[j + 2] as u16 & 0x1F) << 8) | p[j + 3] as u16;
        if prog != 0 {
            st.pmt_pid = Some(pmt_pid);
            break;
        }
        j += 4;
    }
}

fn parse_pmt(p: &[u8], st: &mut TsState) {
    if p.is_empty() {
        return;
    }
    let start = 1 + p[0] as usize;
    if start + 12 > p.len() {
        return;
    }
    if p[start] != 0x02 {
        return;
    }
    let sec_len = (((p[start + 1] & 0x0F) as usize) << 8) | p[start + 2] as usize;
    let end = start + 3 + sec_len;
    let prog_info_len = (((p[start + 10] & 0x0F) as usize) << 8) | p[start + 11] as usize;
    let mut j = start + 12 + prog_info_len;
    while j + 5 <= end && j + 5 <= p.len() {
        let stream_type = p[j];
        let el_pid = (((p[j + 1] as u16) & 0x1F) << 8) | p[j + 2] as u16;
        let es_info_len = (((p[j + 3] as u16) & 0x0F) << 8) | p[j + 4] as u16;
        if matches!(stream_type, 0x0F | 0x03 | 0x04) {
            st.audio_pid = Some(el_pid);
            break;
        }
        j += 5 + es_info_len as usize;
    }
}

/// Indica si un segmento HLS es un flujo MPEG-TS (paquetes de 188 bytes que
/// empiezan por `0x47`). Muchas emisoras HLS sirven en su lugar un flujo
/// elemental directo (p. ej. ADTS AAC), que hay que entregar tal cual al
/// decodificador sin intentar demultiplexarlo.
fn es_mpeg_ts(data: &[u8]) -> bool {
    const PKT: usize = 188;
    // Se exige ver el byte de sincronía al inicio de cada paquete completo que
    // quepa en el segmento: así no se confunde un flujo elemental (que puede
    // contener un `0x47` suelto) con TS. Se ignoran los bytes finales de un
    // posible paquete truncado.
    if data.len() < PKT || data[0] != 0x47 {
        return false;
    }
    let mut i = PKT;
    while i + PKT <= data.len() {
        if data[i] != 0x47 {
            return false;
        }
        i += PKT;
    }
    true
}

/// Longitud total de una etiqueta ID3v2 (cabecera de 10 bytes + tamaño
/// "synchsafe" de 4 bytes) si `data` empieza por una; si no, `0`.
fn tamano_id3(data: &[u8]) -> usize {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return 0;
    }
    // El tamaño usa 7 bits por byte (nunca el bit alto).
    let size = ((data[6] as usize & 0x7F) << 21)
        | ((data[7] as usize & 0x7F) << 14)
        | ((data[8] as usize & 0x7F) << 7)
        | (data[9] as usize & 0x7F);
    (10 + size).min(data.len())
}

/// Entrega un segmento HLS al `out`: si es MPEG-TS se demultiplexa y, si ya es
/// un flujo elemental (p. ej. ADTS AAC), se concatena descartando una posible
/// etiqueta ID3 inicial (habitual en este tipo de segmentos), ya que si no
/// quedaría incrustada en medio del flujo continuo y rompería la decodificación.
fn demux_segmento(data: &[u8], st: &mut TsState, out: &mut Vec<u8>) {
    if es_mpeg_ts(data) {
        demux_ts(data, st, out);
    } else {
        out.extend_from_slice(&data[tamano_id3(data)..]);
    }
}

fn demux_ts(data: &[u8], st: &mut TsState, out: &mut Vec<u8>) {
    let mut i = 0;
    while i + 188 <= data.len() {
        if data[i] != 0x47 {
            i += 1;
            continue;
        }
        let pusi = (data[i + 1] & 0x40) != 0;
        let pid = (((data[i + 1] & 0x1F) as u16) << 8) | (data[i + 2] as u16);
        let afc = (data[i + 3] >> 4) & 0x03;
        let mut off = i + 4;
        if afc & 0x02 != 0 {
            // Longitud del campo de adaptación. Debe caber dentro del paquete;
            // si no (dato corrupto o desalineado) se descarta el paquete.
            let al = data[off] as usize;
            off += 1 + al;
            if off > i + 188 {
                i += 188;
                continue;
            }
        }
        if afc & 0x01 == 0 {
            i += 188;
            continue;
        }
        let payload = &data[off..i + 188];
        if pid == 0 {
            parse_pat(payload, st);
        } else if Some(pid) == st.pmt_pid {
            parse_pmt(payload, st);
        } else if Some(pid) == st.audio_pid {
            if pusi {
                // PES: start code(3) + stream_id(1) + len(2) + flags(2) +
                // PES_header_data_length(1) + PES_header_data(payload[8] bytes).
                // El flujo elemental empieza en 9 + PES_header_data_length.
                if payload.len() >= 9 {
                    let hdr = 9 + payload[8] as usize;
                    if hdr <= payload.len() {
                        out.extend_from_slice(&payload[hdr..]);
                    }
                }
            } else {
                out.extend_from_slice(payload);
            }
        }
        i += 188;
    }
}

/// Descarga texto (playlist) con reintentos y backoff.
/// `max_intentos`:
///   - `None`: reintenta indefinidamente (recargas durante la reproducción).
///   - `Some(n)`: se rinde tras `n` fallos consecutivos (conexión inicial).
///
/// En ambos casos un error de cliente (4xx) es definitivo y no se reintenta.
fn http_get_text_intentos(
    client: &Client,
    url: &str,
    alive: &Arc<AtomicBool>,
    max_intentos: Option<u32>,
) -> Option<String> {
    let mut delay = Duration::from_millis(500);
    let mut intentos = 0u32;
    loop {
        if !alive.load(Ordering::Relaxed) {
            return None;
        }
        match client.get(url).send() {
            Ok(resp) if resp.status().is_success() => return resp.text().ok(),
            Ok(resp) => {
                let st = resp.status();
                log!("hls", "respuesta inesperada: {st}");
                if st.is_client_error() {
                    return None;
                }
            }
            Err(e) => log!("hls", "fallo de descarga: {e}"),
        }
        intentos += 1;
        if let Some(max) = max_intentos {
            if intentos >= max {
                return None;
            }
        }
        if !alive.load(Ordering::Relaxed) {
            return None;
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// Descarga los bytes de un segmento con reintentos y backoff.
fn http_get_bytes(client: &Client, url: &str, alive: &Arc<AtomicBool>) -> Option<Vec<u8>> {
    let mut delay = Duration::from_millis(500);
    loop {
        if !alive.load(Ordering::Relaxed) {
            return None;
        }
        match client.get(url).send() {
            Ok(resp) if resp.status().is_success() => {
                return resp.bytes().ok().map(|b| b.to_vec());
            }
            Ok(resp) => log!("hls", "respuesta inesperada: {}", resp.status()),
            Err(e) => log!("hls", "fallo de descarga: {e}"),
        }
        if !alive.load(Ordering::Relaxed) {
            return None;
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// Playlist media HLS ya resuelta. Los segmentos se identifican por su número de
/// secuencia absoluto `base_seq + índice`, lo que permite saber si un segmento
/// es el siguiente que toca o un fragmento ya reproducido (evita repetirlos al
/// recargar la playlist en las emisoras *live* de ventana deslizante).
struct MediaPlaylist {
    base_seq: i64,
    endlist: bool,
    segments: Vec<String>,
    discont: Vec<bool>,
}

/// Parsea una playlist media HLS: número de secuencia base, marcas de final y
/// de discontinuidad, y las URLs de los segmentos (resueltas contra `base`).
/// Devuelve `None` si todavía no contiene ningún segmento.
fn parse_media_playlist(body: &str, base: &str) -> Option<MediaPlaylist> {
    let mut base_seq: i64 = 0;
    let mut endlist = false;
    let mut segments = Vec::new();
    let mut discont = Vec::new();
    let mut pending_discont = false;
    for raw in body.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            base_seq = rest.trim().parse().unwrap_or(0);
        } else if line == "#EXT-X-ENDLIST" {
            endlist = true;
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_discont = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            segments.push(HlsReader::join_url(base, line));
            discont.push(pending_discont);
            pending_discont = false;
        }
    }
    if segments.is_empty() {
        None
    } else {
        Some(MediaPlaylist {
            base_seq,
            endlist,
            segments,
            discont,
        })
    }
}

/// Resultado de un *prefetch* de segmento HLS: número de secuencia y bytes.
type SegmentoDescargado = (i64, Vec<u8>);

/// Tarea de descarga anticipada en segundo plano (secuencia y su hilo).
type Prefetch = (i64, JoinHandle<Option<SegmentoDescargado>>);

/// Lector HLS: descarga la playlist, va obteniendo los segmentos y demuxa su
/// TS para entregar un flujo continuo de bytes de audio elemental. La posición
/// se sigue por número de secuencia (nunca hacia atrás) y se descarga el
/// siguiente segmento en segundo plano (prefetch) para encadenarlos sin huecos
/// de red apreciables entre segmentos.
struct HlsReader {
    playlist_url: String,
    client: Client,
    pl: Option<MediaPlaylist>,
    next_seq: i64,
    pending: Vec<u8>,
    prefetch: Option<Prefetch>,
    ts: TsState,
    alive: Arc<AtomicBool>,
}

impl HlsReader {
    fn new(playlist_url: String, alive: Arc<AtomicBool>, client: Client) -> io::Result<Self> {
        let mut h = Self {
            playlist_url,
            client,
            pl: None,
            next_seq: 0,
            pending: Vec::new(),
            prefetch: None,
            ts: TsState::new(),
            alive,
        };
        h.reload_playlist_inicial()?;
        Ok(h)
    }

    fn join_url(base: &str, seg: &str) -> String {
        if seg.starts_with("http://") || seg.starts_with("https://") {
            return seg.to_string();
        }
        if let Some(pos) = base.rfind('/') {
            let dir = &base[..=pos];
            format!("{dir}{seg}")
        } else {
            seg.to_string()
        }
    }

    /// Carga inicial de la playlist con reintentos acotados: evita quedarse en
    /// «Conectando…» indefinidamente si la URL está caída.
    fn reload_playlist_inicial(&mut self) -> io::Result<()> {
        self.reload_playlist_con(Some(MAX_INTENTOS_CONEXION))
    }

    /// Recarga de la playlist durante la reproducción (reintentos sin límite).
    fn reload_playlist(&mut self) -> io::Result<()> {
        self.reload_playlist_con(None)
    }

    fn reload_playlist_con(&mut self, max_intentos: Option<u32>) -> io::Result<()> {
        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelado"));
            }
            let body = match http_get_text_intentos(
                &self.client,
                &self.playlist_url,
                &self.alive,
                max_intentos,
            ) {
                Some(b) => b,
                None if !self.alive.load(Ordering::Relaxed) => {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelado"));
                }
                None => {
                    return Err(io::Error::other(
                        "no se pudo descargar la lista de la emisora",
                    ));
                }
            };
            if body.contains("#EXT-X-STREAM-INF") {
                // Playlist maestra: elegir la primera variante y usarla como media playlist.
                for line in body.lines() {
                    let l = line.trim();
                    if l.starts_with("#EXT-X-STREAM-INF") {
                        continue;
                    }
                    if !l.is_empty() && !l.starts_with('#') {
                        let variant = Self::join_url(&self.playlist_url, l);
                        self.playlist_url = variant;
                        break;
                    }
                }
                continue; // volver a descargar la playlist de la variante
            }
            if let Some(pl) = parse_media_playlist(&body, &self.playlist_url) {
                self.apply_playlist(pl);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Aplica una playlist recién descargada manteniendo la posición avanzando
    /// siempre hacia delante (nunca se re-reproducen fragmentos ya oídos). Si el
    /// reproductor quedó retrasado respecto al directo, reanuda en el primer
    /// segmento disponible de la ventana (`base_seq`), dejando el resto de la
    /// ventana como cola: así cada fragmento que se necesita ya está publicado
    /// cuando toca reproducirlo y no se produce corte alguno en las fronteras.
    fn apply_playlist(&mut self, pl: MediaPlaylist) {
        let max_seq = pl.base_seq + pl.segments.len() as i64 - 1;
        if self.next_seq < pl.base_seq {
            log!(
                "hls",
                "re-sincronizado: estaba en {} y la ventana empieza en {}; reanudando en el inicio de la ventana",
                self.next_seq,
                pl.base_seq
            );
            self.next_seq = pl.base_seq;
        } else if self.next_seq > max_seq {
            log!(
                "hls",
                "al día: next_seq={} fuera de la ventana (max={}); esperando segmento",
                self.next_seq,
                max_seq
            );
        }
        self.pl = Some(pl);
    }

    /// URL del segmento con número de secuencia `seq`, si está en la ventana
    /// actual. `None` si ya caducó o todavía no ha sido publicado.
    fn segment_for_seq(&self, seq: i64) -> Option<String> {
        let pl = self.pl.as_ref()?;
        if seq < pl.base_seq {
            return None;
        }
        let idx = (seq - pl.base_seq) as usize;
        pl.segments.get(idx).cloned()
    }

    /// Indica si el segmento `seq` arrastra una marca `#EXT-X-DISCONTINUITY`
    /// (cambio de codificación), en cuyo caso hay que reiniciar el demuxado TS.
    fn segment_discont(&self, seq: i64) -> bool {
        match &self.pl {
            Some(pl) if seq >= pl.base_seq => {
                let idx = (seq - pl.base_seq) as usize;
                pl.discont.get(idx).copied().unwrap_or(false)
            }
            _ => false,
        }
    }

    /// Descarga el siguiente segmento (secuencia `next_seq`), recargando la
    /// playlist hasta que exista. Da `None` al cancelar o al final de un VOD ya
    /// reproducido por completo (`#EXT-X-ENDLIST`).
    fn download_segment(&mut self) -> Option<(i64, Vec<u8>)> {
        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(pl) = &self.pl {
                let max_seq = pl.base_seq + pl.segments.len() as i64 - 1;
                if pl.endlist && self.next_seq > max_seq {
                    return None;
                }
                if self.next_seq >= pl.base_seq && self.next_seq <= max_seq {
                    let idx = (self.next_seq - pl.base_seq) as usize;
                    let url = pl.segments[idx].clone();
                    if let Some(bytes) = http_get_bytes(&self.client, &url, &self.alive) {
                        let seq = self.next_seq;
                        self.next_seq += 1;
                        return Some((seq, bytes));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
            if self.reload_playlist().is_err() {
                return None;
            }
        }
    }

    /// Lanza un hilo que descarga el segmento `next_seq` (por URL concreta, sin
    /// tocar la playlist) para tenerlo listo mientras se consume el actual.
    fn spawn_prefetch(&mut self) {
        let seq = self.next_seq;
        let url = match self.segment_for_seq(seq) {
            Some(u) => u,
            None => return, // aún no publicado: download_segment lo obtendrá al recargar
        };
        let client = self.client.clone();
        let alive = self.alive.clone();
        let handle = std::thread::spawn(move || {
            capturar("prefetch", move || {
                let mut delay = Duration::from_millis(500);
                loop {
                    if !alive.load(Ordering::Relaxed) {
                        return None;
                    }
                    if let Some(bytes) = http_get_bytes(&client, &url, &alive) {
                        return Some((seq, bytes));
                    }
                    if !alive.load(Ordering::Relaxed) {
                        return None;
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            })
            .flatten()
        });
        self.prefetch = Some((seq, handle));
    }

    /// Rellena `pending` con el siguiente segmento, aprovechando el prefetch si
    /// está disponible. Descartan en fragmentos ya reproducidos (secuencia
    /// anterior a `next_seq`), de modo que una playlist movida o un prefetch
    /// caduco nunca causan repeticiones.
    fn fill(&mut self) {
        while self.pending.is_empty() {
            if !self.alive.load(Ordering::Relaxed) {
                return;
            }
            if let Some((_pseq, handle)) = self.prefetch.take() {
                // `Ok(Some(..))` = segmento listo; `Ok(None)`/`Err` = el prefetch
                // falló y se seguirá el camino síncrono de `download_segment`.
                if let Ok(Some((seq, bytes))) = handle.join() {
                    if seq >= self.next_seq {
                        if self.segment_discont(seq) {
                            log!("hls", "discontinuidad en seq {seq}; reiniciando demux");
                            self.ts = TsState::new();
                        }
                        if seq > self.next_seq {
                            log!("hls", "salto de secuencia {} -> {}", self.next_seq, seq);
                        }
                        self.next_seq = seq + 1;
                        demux_segmento(&bytes, &mut self.ts, &mut self.pending);
                        self.spawn_prefetch();
                        continue;
                    }
                    log!("hls", "descartando fragmento ya reproducido (seq {seq})");
                    self.spawn_prefetch();
                    continue;
                }
            }
            match self.download_segment() {
                Some((seq, bytes)) => {
                    if self.segment_discont(seq) {
                        log!("hls", "discontinuidad en seq {seq}; reiniciando demux");
                        self.ts = TsState::new();
                    }
                    demux_segmento(&bytes, &mut self.ts, &mut self.pending);
                    self.spawn_prefetch();
                }
                None => {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
}

impl Read for HlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            self.fill();
        }
        if self.pending.is_empty() {
            return Ok(0);
        }
        let n = min(buf.len(), self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Normalizador ADTS
// ---------------------------------------------------------------------------

/// Tamaño mínimo (solo cabecera) y máximo de una trama ADTS.
const ADTS_MIN_FRAME: usize = 7;
const ADTS_MAX_FRAME: usize = 8191;
/// Máximo de bytes que se acumulan antes de concluir que el flujo no es ADTS.
const ADTS_DETECCION_MAX: usize = 16 * 1024;

/// Devuelve el tamaño total de la trama si en `d[p..]` hay una cabecera ADTS
/// válida **sin CRC** (MPEG-4 `0xFF F1` o MPEG-2 `0xFF F9`). Se descartan las
/// frecuencias de muestreo prohibidas (13, 14 y 15) para reducir falsos
/// positivos.
fn adts_frame_len(d: &[u8], p: usize) -> Option<usize> {
    if p + 7 > d.len() || d[p] != 0xFF || (d[p + 1] != 0xF1 && d[p + 1] != 0xF9) {
        return None;
    }
    let sf = (d[p + 2] >> 2) & 0x0F;
    if sf >= 13 {
        return None;
    }
    let len = (((d[p + 3] & 0x03) as usize) << 11)
        | ((d[p + 4] as usize) << 3)
        | ((d[p + 5] as usize) >> 5);
    if (ADTS_MIN_FRAME..=ADTS_MAX_FRAME).contains(&len) {
        Some(len)
    } else {
        None
    }
}

/// Busca el primer desplazamiento con `tramas` cabeceras ADTS consecutivas y
/// alineadas. Exigir más de una trama reduce mucho los falsos positivos (p. ej.
/// un `0xFF 0xF1` casual dentro del audio MP3).
fn adts_cadena(d: &[u8], tramas: usize) -> Option<usize> {
    let mut p = 0;
    'buscar: while p + ADTS_MIN_FRAME <= d.len() {
        if adts_frame_len(d, p).is_some() {
            let mut q = p;
            for _ in 0..tramas {
                match adts_frame_len(d, q) {
                    Some(l) => q += l,
                    None => {
                        p += 1;
                        continue 'buscar;
                    }
                }
            }
            return Some(p);
        }
        p += 1;
    }
    None
}

/// Estado del normalizador ADTS.
enum ModoAdts {
    /// Aún no se sabe si el flujo es ADTS; se acumulan bytes para decidir.
    Detectando,
    /// Flujo ADTS confirmado: se encuadra trama a trama.
    Adts,
    /// El flujo no es ADTS: se deja pasar tal cual.
    PasoDirecto,
    /// El flujo ha terminado.
    Fin,
}

/// Normaliza el flujo elemental que se entrega a `symphonia`:
///
/// - **Detecta** si es ADTS AAC buscando una cadena de dos tramas consecutivas.
///   Al conectar a mitad de trama (habitual en emisoras Icecast) se descartan
///   los bytes iniciales incompletos hasta la primera cabecera real.
/// - **Convierte** las cabeceras ADTS MPEG-2 (`0xFF F9`, típicas de los flujos
///   `audio/aacp`/HE-AAC de Icecast) a MPEG-4 (`0xFF F1`), que es el único tipo
///   que reconoce el demuxador ADTS de `symphonia`. Sin esto no se decodifican.
/// - Si el flujo **no es ADTS** (p. ej. MP3), se deja pasar sin tocarlo.
struct AdtsNormalizer {
    /// Datos crudos pendientes de encuadrar.
    pend: Vec<u8>,
    /// Datos ya normalizados pendientes de entregar.
    out: VecDeque<u8>,
    modo: ModoAdts,
}

impl AdtsNormalizer {
    fn new() -> Self {
        Self {
            pend: Vec::new(),
            out: VecDeque::new(),
            modo: ModoAdts::Detectando,
        }
    }

    /// Añade datos crudos recién leídos del flujo.
    fn push(&mut self, datos: &[u8]) {
        if matches!(self.modo, ModoAdts::PasoDirecto) {
            self.out.extend(datos.iter().copied());
            return;
        }
        self.pend.extend_from_slice(datos);
        self.procesar();
    }

    /// Indica que el flujo ha terminado: vuelca lo pendiente y cierra.
    fn fin(&mut self) {
        self.procesar();
        if !matches!(self.modo, ModoAdts::PasoDirecto | ModoAdts::Fin) {
            self.out.extend(self.pend.drain(..));
        }
        self.modo = ModoAdts::Fin;
    }

    /// Copia a `buf` los bytes normalizados disponibles y devuelve cuántos.
    fn sacar(&mut self, buf: &mut [u8]) -> usize {
        let n = min(buf.len(), self.out.len());
        for (slot, b) in buf[..n].iter_mut().zip(self.out.iter()) {
            *slot = *b;
        }
        self.out.drain(..n);
        n
    }

    /// `true` cuando el flujo terminó y no queda nada por entregar.
    fn terminado(&self) -> bool {
        matches!(self.modo, ModoAdts::Fin) && self.out.is_empty()
    }

    /// Encuadra los bytes pendientes y los mueve a la salida normalizada.
    fn procesar(&mut self) {
        loop {
            match self.modo {
                ModoAdts::Fin => return,
                ModoAdts::PasoDirecto => {
                    self.out.extend(self.pend.drain(..));
                    return;
                }
                ModoAdts::Detectando => {
                    if let Some(p) = adts_cadena(&self.pend, 2) {
                        if p > 0 {
                            log!("audio", "ADTS detectado: descartados {p} bytes iniciales");
                        }
                        self.pend.drain(..p);
                        self.modo = ModoAdts::Adts;
                        continue;
                    }
                    if self.pend.len() >= ADTS_DETECCION_MAX {
                        log!("audio", "flujo no ADTS: se entrega sin normalizar");
                        self.modo = ModoAdts::PasoDirecto;
                        continue;
                    }
                    return;
                }
                ModoAdts::Adts => {
                    if self.pend.len() < ADTS_MIN_FRAME {
                        return;
                    }
                    if adts_frame_len(&self.pend, 0).is_none() {
                        // Desincronizado: se busca de nuevo una cadena válida.
                        if let Some(p) = adts_cadena(&self.pend, 2) {
                            self.pend.drain(..p);
                            continue;
                        }
                        // Se conservan los últimos bytes por si la cabecera está
                        // partida entre dos lecturas.
                        let conservar = min(ADTS_MIN_FRAME - 1, self.pend.len());
                        let descartar = self.pend.len() - conservar;
                        self.pend.drain(..descartar);
                        return;
                    }
                    let len = adts_frame_len(&self.pend, 0).unwrap();
                    if self.pend.len() < len {
                        return;
                    }
                    // MPEG-2 (0xFF F9) → MPEG-4 (0xFF F1).
                    if self.pend[1] == 0xF9 {
                        self.pend[1] = 0xF1;
                    }
                    self.out.extend(self.pend.drain(..len));
                }
            }
        }
    }
}

/// Lector de alto nivel que resuelve el tipo de URL (playlist HLS/M3U/PLS o
/// flujo directo) y delega la lectura al lector adecuado.
enum Inner {
    Direct(ReconnectingReader),
    Hls(HlsReader),
}

struct RadioReader {
    inner: Inner,
    /// Normalizador ADTS: encuadra el flujo y convierte ADTS MPEG-2 en MPEG-4
    /// antes de entregarlo a `symphonia`.
    adts: AdtsNormalizer,
}

impl RadioReader {
    fn resolve(
        url: &str,
        alive: Arc<AtomicBool>,
        client: &Client,
        depth: u32,
    ) -> io::Result<Box<dyn MediaSource>> {
        if depth > 6 {
            return Err(io::Error::other(
                "demasiados redirecciones de playlist",
            ));
        }
        let lower = url.to_lowercase();

        if lower.ends_with(".m3u8") || lower.contains("m3u8") {
            let r = HlsReader::new(url.to_string(), alive, client.clone())?;
            return Ok(Box::new(RadioReader {
                inner: Inner::Hls(r),
                adts: AdtsNormalizer::new(),
            }));
        }

        if lower.ends_with(".pls") {
            let body = get_text_via(client, url, &alive)?;
            if let Some(resolved) = parse_pls(&body).into_iter().next() {
                return Self::resolve(&resolved, alive, client, depth + 1);
            }
            return Err(io::Error::other("PLS sin URLs válidas"));
        }

        if lower.ends_with(".m3u") {
            let body = get_text_via(client, url, &alive)?;
            if body.contains("#EXT-X-STREAM-INF")
                || body.lines().any(|l| {
                    let t = l.trim_end();
                    t.ends_with(".m3u8") || t.ends_with(".ts")
                })
            {
                let r = HlsReader::new(url.to_string(), alive, client.clone())?;
                return Ok(Box::new(RadioReader {
                    inner: Inner::Hls(r),
                    adts: AdtsNormalizer::new(),
                }));
            }
            if let Some(resolved) = parse_m3u(&body).into_iter().next() {
                return Self::resolve(&resolved, alive, client, depth + 1);
            }
            return Err(io::Error::other("M3U sin URLs válidas"));
        }

        // Flujo directo (MP3/AAC). Se conecta ya aquí para poder informar con un
        // mensaje claro si la emisora está caída (404/403) o inalcanzable, en
        // lugar de dejar la interfaz en «Conectando…» indefinidamente.
        let mut r = ReconnectingReader::new(url.to_string(), alive, client.clone());
        r.connect()?;
        Ok(Box::new(RadioReader {
            inner: Inner::Direct(r),
            adts: AdtsNormalizer::new(),
        }))
    }
}

/// Descarga una playlist de texto durante la resolución de la URL. Reintentos
/// acotados: si la URL está caída se informa del error en vez de reintentar sin
/// fin (la reproducción aún no ha empezado).
fn get_text_via(client: &Client, url: &str, alive: &Arc<AtomicBool>) -> io::Result<String> {
    match http_get_text_intentos(client, url, alive, Some(MAX_INTENTOS_CONEXION)) {
        Some(body) => Ok(body),
        None if !alive.load(Ordering::Relaxed) => {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelado"))
        }
        None => Err(io::Error::other("no se pudo descargar la lista de la emisora")),
    }
}

fn parse_m3u(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

fn parse_pls(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|l| l.to_lowercase().starts_with("file") && l.contains('='))
        .map(|l| l.split_once('=').map(|x| x.1).unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

impl Read for RadioReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // Se entrega primero lo ya normalizado.
            let n = self.adts.sacar(buf);
            if n > 0 {
                return Ok(n);
            }
            if self.adts.terminado() {
                return Ok(0);
            }
            // Se leen más datos crudos del lector interno (directo o HLS).
            let mut tmp = [0u8; 16 * 1024];
            let leido = match &mut self.inner {
                Inner::Direct(r) => r.read(&mut tmp)?,
                Inner::Hls(h) => h.read(&mut tmp)?,
            };
            if leido == 0 {
                self.adts.fin();
            } else {
                self.adts.push(&tmp[..leido]);
            }
        }
    }
}

impl Seek for RadioReader {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::other("seek no soportado"))
    }
}

impl MediaSource for RadioReader {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Fuente de audio que usa `symphonia` para demuxar/decodificar el flujo
/// (MP3 o AAC) y entrega las muestras PCM (f32 en [-1,1]) a rodio, que se
/// encarga de la reproducción y del remuestreo a la frecuencia del dispositivo.
struct RadioSource {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn Decoder>,
    channels: u16,
    sample_rate: u32,
    frame_len: usize,
    buffer: Vec<f32>,
    pos: usize,
    decode_errors: u32,
    alive: Arc<AtomicBool>,
}

impl RadioSource {
    fn new(reader: Box<dyn MediaSource>, alive: Arc<AtomicBool>) -> io::Result<Self> {
        let mss = MediaSourceStream::new(reader, Default::default());

        let probed: ProbeResult = get_probe()
            .format(
                &Hint::new(),
                mss,
                &symphonia::core::formats::FormatOptions::default(),
                &symphonia::core::meta::MetadataOptions::default(),
            )
            .map_err(|e| {
                // El detalle queda en el log (stderr); al usuario se le muestra
                // un mensaje claro.
                log!("dec", "fallo al analizar el formato: {e}");
                match e {
                    SError::IoError(io_e) => io_e,
                    _ => io::Error::other("no se reconoce el formato del flujo de audio"),
                }
            })?;

        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| io::Error::other("sin pista de audio"))?;

        let codec_params = &track.codec_params;
        let channels = codec_params.channels.map(|c| c.count() as u16).unwrap_or(2);
        let sample_rate = codec_params.sample_rate.unwrap_or(44100);

        let decoder = get_codecs()
            .make(codec_params, &DecoderOptions::default())
            .map_err(|e| io::Error::other(format!("decoder: {e}")))?;

        let mut s = Self {
            format,
            decoder,
            channels,
            sample_rate,
            frame_len: 1024,
            buffer: Vec::new(),
            pos: 0,
            decode_errors: 0,
            alive,
        };
        // Decodificar el primer paquete para fijar canales y tasa de muestreo
        // reales del flujo ANTES de que rodio monte el remuestreador (evita
        // arrancarlo con 44100 por defecto y reproducir HLS 48 kHz en falso).
        s.refill();
        Ok(s)
    }

    /// Decodifica el siguiente paquete y rellena el búfer de muestras. Si hay
    /// un error de red/decodificación, espera y reintenta indefinidamente (la
    /// reconexión ocurre dentro del lector) en vez de terminar la fuente.
    fn refill(&mut self) {
        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return;
            }
            match self.format.next_packet() {
                Ok(packet) => match self.decoder.decode(&packet) {
                    Ok(decoded) => {
                        self.decode_errors = 0;
                        let spec: SignalSpec = *decoded.spec();
                        self.channels = spec.channels.count() as u16;
                        self.sample_rate = spec.rate;
                        let frames = decoded.capacity() as u64;
                        let mut sb = SampleBuffer::<f32>::new(frames, spec);
                        sb.copy_interleaved_ref(decoded);
                        self.buffer = sb.samples().to_vec();
                        self.frame_len = self.buffer.len().max(1);
                        self.pos = 0;
                        if !self.buffer.is_empty() {
                            return;
                        }
                    }
                    Err(e) => {
                        self.decode_errors += 1;
                        // Errores repetidos suelen indicar un cambio de formato en
                        // el flujo (continuidad rota en HLS: el decodificador debe
                        // recrearse con los parámetros vigentes del paquete).
                        if self.decode_errors >= 4 {
                            self.decode_errors = 0;
                            let track_id = packet.track_id();
                            if let Some(params) = self
                                .format
                                .tracks()
                                .iter()
                                .find(|t| t.id == track_id)
                                .map(|t| t.codec_params.clone())
                            {
                                if let Ok(dec) =
                                    get_codecs().make(&params, &DecoderOptions::default())
                                {
                                    self.decoder = dec;
                                    log!("dec", "decodificador reinicializado tras: {e}");
                                }
                            }
                        }
                        log!("dec", "error de decodificación: {e}, reintentando...");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                },
                Err(SError::IoError(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log!("dec", "error de paquete: {e}, reintentando...");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
}

impl Iterator for RadioSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos >= self.buffer.len() {
            if !self.alive.load(Ordering::Relaxed) {
                return None;
            }
            self.refill();
            if self.pos >= self.buffer.len() {
                return None;
            }
        }
        let s = self.buffer[self.pos];
        self.pos += 1;
        Some(s)
    }
}

impl Source for RadioSource {
    fn current_frame_len(&self) -> Option<usize> {
        Some(self.frame_len)
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Estado de reproducción que el reproductor comparte con la interfaz gráfica
/// para que esta pueda informar al usuario de lo que está ocurriendo (en vez de
/// mostrar siempre el mismo texto aunque no haya sonido).
#[derive(Clone, Debug, PartialEq)]
pub enum EstadoReproduccion {
    /// Sin emisora activa (parado o aún sin arrancar).
    Detenido,
    /// Resolviendo la URL y esperando la primera trama de audio.
    Conectando,
    /// Audio en reproducción.
    Reproduciendo,
    /// No se pudo iniciar la reproducción (dispositivo, URL, formato…).
    Error(String),
}

/// Fija el estado de reproducción compartido, ignorando el *poisoning* del
/// mutex (si un hilo entra en pánico no queremos tumbar la interfaz).
fn set_estado(estado: &Arc<Mutex<EstadoReproduccion>>, nuevo: EstadoReproduccion) {
    *estado.candado() = nuevo;
}

/// Reproductor que mantiene un `OutputStream` de rodio vivo y permite cambiar
/// de emisora deteniendo la fuente anterior (marcando su bandera `alive`) e
/// inyectando una nueva en el mismo `OutputStreamHandle`. Además permite
/// controlar volumen, pausa/reanudación y detener limpiamente usando un `Sink`.
/// Mensaje que el hilo de conexión envía cuando la fuente ya está lista:
/// bandera de vida, cola de audio y `Sink` de rodio.
type SinkListo = (Arc<AtomicBool>, Arc<AudioQueue>, Sink);

pub struct Player {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    current_alive: Option<Arc<AtomicBool>>,
    /// Última información de metadatos ICY ("now playing") de la emisora.
    metadata: Arc<Mutex<String>>,
    /// Estado de reproducción actual, consultable por la GUI.
    estado: Arc<Mutex<EstadoReproduccion>>,
    /// Sink de rodio asociado a la emisora actual. Permite pausar, reanudar,
    /// cambiar volumen y detener el audio sin necesidad de matar hilos.
    sink: Option<Sink>,
    /// Receptor pendiente del `(alive, cola, sink)` que el hilo de conexión
    /// enviará cuando la fuente esté lista. Como la conexión puede tardar
    /// bastante más que el tiempo que queremos bloquear la UI (HLS, playlists,
    /// redes lentas), el `Sink` se "cosecha" de forma asíncrona en los
    /// siguientes frames en lugar de descartarlo si no llega a tiempo.
    sink_rx: Option<mpsc::Receiver<SinkListo>>,
    volumen: f32,
    pausado: bool,
    queue: Option<Arc<AudioQueue>>,
}

impl Player {
    pub fn new() -> io::Result<Self> {
        let (_stream, handle) =
            OutputStream::try_default().map_err(io::Error::other)?;
        Ok(Self {
            _stream,
            handle,
            current_alive: None,
            metadata: Arc::new(Mutex::new(String::new())),
            estado: Arc::new(Mutex::new(EstadoReproduccion::Detenido)),
            sink: None,
            sink_rx: None,
            volumen: 1.0,
            pausado: false,
            queue: None,
        })
    }

    /// Devuelve los metadatos ICY actuales de la emisora en reproducción.
    pub fn get_metadata(&self) -> String {
        self.metadata.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Devuelve el estado de reproducción actual (para que la GUI lo muestre).
    pub fn get_estado(&self) -> EstadoReproduccion {
        self.estado
            .lock()
            .map(|g| g.clone())
            .unwrap_or(EstadoReproduccion::Detenido)
    }

    /// Devuelve el volumen actual aplicado al `Sink` (0.0 – 2.0 aprox.).
    pub fn get_volumen(&self) -> f32 {
        self.volumen
    }

    /// Fija el volumen del reproductor. Se clampea a un rango razonable
    /// para evitar valores extremos.
    pub fn set_volumen(&mut self, v: f32) {
        let v = v.clamp(0.0, 2.0);
        self.volumen = v;
        if let Some(sink) = self.sink.as_mut() {
            sink.set_volume(v);
        }
    }

    /// Porcentaje de volumen para mostrar en la interfaz gráfica (0–200 %).
    #[allow(dead_code)]
    pub fn get_volumen_pct(&self) -> u32 {
        ((self.volumen * 100.0).round() as u32).min(200)
    }

    /// Indica si el audio está pausado en este momento.
    pub fn is_pausado(&self) -> bool {
        self.pausado
    }

    /// Alterna entre pausa y reanudación. Si no hay `Sink` activo, la pausa
    /// queda memorizada para aplicarla cuando se cree el siguiente `Sink`.
    #[allow(dead_code)]
    pub fn toggle_pausa(&mut self) {
        if let Some(q) = self.queue.as_mut() {
            q.set_pausado(!self.pausado);
        }
        if let Some(sink) = self.sink.as_mut() {
            if sink.is_paused() {
                sink.play();
                self.pausado = false;
            } else {
                sink.pause();
                self.pausado = true;
            }
        } else {
            self.pausado = !self.pausado;
        }
    }

    /// Pausa la reproducción si hay un `Sink` activo; en caso contrario deja
    /// la pausa memorizada para el siguiente inicio.
    pub fn pausar(&mut self) {
        if let Some(q) = self.queue.as_mut() {
            q.set_pausado(true);
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.pause();
        }
        self.pausado = true;
    }

    /// Reanuda la reproducción si hay un `Sink` activo; en caso contrario borra
    /// la pausa memorizada.
    pub fn reanudar(&mut self) {
        if let Some(q) = self.queue.as_mut() {
            q.set_pausado(false);
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.play();
        }
        self.pausado = false;
    }

    /// Detiene la emisora actual (si la hay) y reproduce `url` en un hilo
    /// aparte, para no bloquear la interfaz gráfica mientras se resuelve la
    /// URL (playlist/HLS) y se espera la primera trama del stream. Además
    /// lanza un hilo paralelo que lee los metadatos ICY en vivo y publica el
    /// estado de reproducción en `estado`.
    pub fn play(&mut self, url: String) {
        // Detenemos cualquier reproducción anterior (Sink + bandera alive).
        if let Some(old) = self.current_alive.take() {
            old.store(false, Ordering::Relaxed);
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.stop();
        }
        self.sink = None;
        // Descarta cualquier `Sink` pendiente de la emisora anterior y deja de
        // pausar la cola antigua.
        self.sink_rx = None;
        if let Some(q) = self.queue.take() {
            q.set_pausado(false);
        }

        let alive = Arc::new(AtomicBool::new(true));
        self.current_alive = Some(alive.clone());
        set_estado(&self.estado, EstadoReproduccion::Conectando);

        let (tx, rx) = mpsc::channel::<SinkListo>();
        let url_audio = url.clone();
        let alive_audio = alive.clone();
        let alive_estado = alive.clone();
        let estado = self.estado.clone();
        let handle = self.handle.clone();
        let volumen = self.volumen;
        let pausado = self.pausado;

        std::thread::spawn(move || {
            let estado_panico = estado.clone();
            let alive_panico = alive_estado.clone();
            let panico = ejecutar_protegido("conexion", move || {
                match iniciar_fuente(url_audio, alive_audio.clone()) {
                    Ok(fuente) => {
                        let cola = fuente.queue.clone();
                        match Sink::try_new(&handle).map_err(io::Error::other) {
                            Ok(sink) => {
                                sink.set_volume(volumen);
                                if pausado {
                                    sink.pause();
                                } else {
                                    sink.play();
                                }
                                sink.append(fuente);
                                let _ = tx.send((alive_estado.clone(), cola, sink));
                            }
                            Err(e) => {
                                log!("audio", "error al crear Sink: {e}");
                                if alive_estado.load(Ordering::Relaxed) {
                                    set_estado(&estado, EstadoReproduccion::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log!("audio", "error de reproducción: {e}");
                        if alive_estado.load(Ordering::Relaxed) {
                            set_estado(&estado, EstadoReproduccion::Error(e.to_string()));
                        }
                    }
                }
            });
            if panico && alive_panico.load(Ordering::Relaxed) {
                set_estado(
                    &estado_panico,
                    EstadoReproduccion::Error("fallo interno al conectar".to_string()),
                );
            }
        });

        // Guardamos el receptor para cosechar el `Sink` cuando esté listo. Se
        // intenta ya mismo (bloqueando como mucho `espera`) y, si aún no ha
        // llegado, se seguirá intentando en los siguientes frames de la UI.
        self.sink_rx = Some(rx);
        self.cosechar_sink(Some(std::time::Duration::from_millis(500)));

        // Hilo de metadatos ICY
        let m_alive = alive.clone();
        let m_meta = self.metadata.clone();
        m_meta.candado().clear();
        std::thread::spawn(move || {
            ejecutar_protegido("metadatos", move || {
                icy_metadata_loop(url, m_alive, m_meta);
            });
        });
    }

    /// Recoge el `Sink` pendiente enviado por el hilo de conexión, si ya está
    /// disponible. Al recibirlo se le aplican el volumen y la pausa actuales y
    /// el estado pasa a `Reproduciendo`. Con `espera = None` no bloquea (se usa
    /// desde la UI en cada frame); con `Some(d)` espera como mucho `d`.
    fn cosechar_sink(&mut self, espera: Option<std::time::Duration>) {
        let rx = match self.sink_rx.take() {
            Some(rx) => rx,
            None => return,
        };
        let recibido = match espera {
            Some(d) => rx.recv_timeout(d).ok(),
            None => rx.try_recv().ok(),
        };
        match recibido {
            Some((alive, cola, sink)) => {
                // Si la emisora ya no interesa (se cambió de estación) se
                // descarta el Sink, que deja de sonar al soltarse.
                if alive.load(Ordering::Relaxed) {
                    sink.set_volume(self.volumen);
                    if self.pausado {
                        sink.pause();
                    } else {
                        sink.play();
                    }
                    cola.set_pausado(self.pausado);
                    self.queue = Some(cola);
                    self.sink = Some(sink);
                    set_estado(&self.estado, EstadoReproduccion::Reproduciendo);
                }
            }
            // Todavía no ha llegado: se conserva el receptor para reintentarlo.
            None => self.sink_rx = Some(rx),
        }
    }

    /// Llamada periódica desde la interfaz para cosechar el `Sink` en cuanto el
    /// hilo de conexión lo tenga listo (sin bloquear).
    pub fn poll(&mut self) {
        self.cosechar_sink(None);
    }

    pub fn stop(&mut self) {
        if let Some(old) = self.current_alive.take() {
            old.store(false, Ordering::Relaxed);
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.stop();
        }
        self.sink = None;
        // Se descarta cualquier `Sink` pendiente de llegar: al soltar el
        // receptor, el `send` del hilo de conexión fallará y el Sink se soltará
        // sin llegar a sonar.
        self.sink_rx = None;
        if let Some(q) = self.queue.take() {
            q.set_pausado(false);
        }
        self.pausado = false;
        set_estado(&self.estado, EstadoReproduccion::Detenido);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Número de muestras de PCM por canal que el decodificador mantiene por
/// delante de la reproducción (~1,5 s). Es la cola que absorbe las esperas de
/// red/fronteras de segmento sin que el dispositivo de audio sufra `underrun`.
const AUDIO_QUEUE_SECONDS: usize = 3;

/// Cola de audio PCM (f32 intercalado) que separa la decodificación (en un
/// hilo de fondo) de la reproducción (pull de rodio). El hilo de audio hace
/// *pop* sin bloquear: si la cola queda momentáneamente vacía se entrega un
/// silencio hasta que el productor la reponga. Mantenerla rellena por delante
/// evita que ALSA sufra `underrun` con las micro-esperas del HLS.
struct AudioQueue {
    estado: Mutex<EstadoCola>,
    aviso: std::sync::Condvar,
    /// Bandera "vivo" de la emisora: al ponerse a `false` (cambio de emisora o
    /// parada) el hilo de audio corta de inmediato, sin drenar la cola.
    alive: Arc<AtomicBool>,
}

struct EstadoCola {
    samples: Vec<f32>,
    head: usize,
    len: usize,
    channels: u16,
    rate: u32,
    cerrada: bool,
    pausado: bool,
}

impl AudioQueue {
    /// Crea la cola para `channels`×`rate` y la fuente de reproducción asociada.
    /// `alive` controla el corte inmediato al cambiar de emisora.
    fn new(channels: u16, rate: u32, alive: Arc<AtomicBool>) -> (Arc<Self>, QueuedSource) {
        let cap = (rate as usize * channels as usize * AUDIO_QUEUE_SECONDS).max(1024);
        let cola = Arc::new(Self {
            estado: Mutex::new(EstadoCola {
                samples: vec![0.0; cap],
                head: 0,
                len: 0,
                channels,
                rate,
                cerrada: false,
                pausado: false,
            }),
            aviso: std::sync::Condvar::new(),
            alive,
        });
        let fuente = QueuedSource {
            queue: cola.clone(),
        };
        (cola, fuente)
    }

    fn watermark(&self, s: &EstadoCola) -> usize {
        // Frena al productor ligeramente por debajo de la capacidad total.
        (s.samples.len() * 9 / 10).max(1)
    }

    /// Inserta un bloque de muestras; si la cola está llena según el objetivo,
    /// espera (sin consumir CPU) a que la reproducción libere hueco. Nunca
    /// descarta audio. Escribe en sub-bloques de 512 muestras para no retener el
    /// mutex durante mucho tiempo (el hilo de audio de rodio hace *pop* con el
    /// mismo lock y conviene que nunca espere).
    fn push_slice(&self, data: &[f32]) {
        let mut idx = 0;
        while idx < data.len() {
            let mut s = self.estado.candado();
            let objetivo = self.watermark(&s);
            while s.len >= objetivo && !s.cerrada && self.alive.load(Ordering::Relaxed) {
                s = self.aviso.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            // Emisora detenida o cola cerrada: descartar el resto sin rellenar.
            if s.cerrada || !self.alive.load(Ordering::Relaxed) {
                s.cerrada = true;
                self.aviso.notify_all();
                return;
            }
            let espacio = objetivo - s.len;
            let n = espacio.min(data.len() - idx).min(512);
            for &m in &data[idx..idx + n] {
                let p = (s.head + s.len) % s.samples.len();
                s.samples[p] = m;
                s.len += 1;
            }
            idx += n;
        }
    }

    /// Consume una muestra (nunca bloquea). El corte es inmediato al cambiar de
    /// emisora (`alive` a `false`) aunque quede audio en la cola; en cambio, al
    /// cerrarse por fin natural del flujo sí se drena el resto. Devuelve `None`
    /// al terminar y un silencio si la cola está momentáneamente vacía pero
    /// sigue activa.
    fn pop(&self) -> Option<f32> {
        let mut s = self.estado.candado();
        if s.cerrada {
            return if s.len == 0 {
                None
            } else {
                let v = s.samples[s.head];
                s.head = (s.head + 1) % s.samples.len();
                s.len -= 1;
                if s.len == self.watermark(&s) - 1 {
                    self.aviso.notify_one();
                }
                Some(v)
            };
        }
        if !self.alive.load(Ordering::Relaxed) {
            // Cambio de emisora/parada: cortar ya, sin drenar la cola.
            self.aviso.notify_all();
            return None;
        }
        if s.pausado {
            return Some(0.0);
        }
        if s.len == 0 {
            return Some(0.0);
        }
        let v = s.samples[s.head];
        s.head = (s.head + 1) % s.samples.len();
        s.len -= 1;
        if s.len == self.watermark(&s) - 1 {
            self.aviso.notify_one();
        }
        Some(v)
    }

    fn channels(&self) -> u16 {
        self.estado.candado().channels
    }

    fn rate(&self) -> u32 {
        self.estado.candado().rate
    }

    /// Marca la cola como cerrada (la fuente terminará al agotar lo pendiente).
    fn close(&self) {
        let mut s = self.estado.candado();
        s.cerrada = true;
        self.aviso.notify_all();
    }

    fn set_pausado(&self, p: bool) {
        let mut s = self.estado.candado();
        s.pausado = p;
    }
}

/// Fuente para rodio que consume de `AudioQueue` sin bloquear nunca el hilo de
/// audio.
struct QueuedSource {
    queue: Arc<AudioQueue>,
}

impl Iterator for QueuedSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.queue.pop()
    }
}

impl Source for QueuedSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.queue.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.queue.rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Bucle de decodificación en segundo plano: consume el `RadioSource` y rellena
/// la cola de audio hasta que se cancele (`alive` a `false`) o el flujo termine.
fn bucle_decodificacion(mut src: RadioSource, queue: Arc<AudioQueue>, alive: Arc<AtomicBool>) {
    let mut chunk: Vec<f32> = Vec::with_capacity(8192);
    loop {
        if !alive.load(Ordering::Relaxed) {
            break;
        }
        match src.next() {
            Some(m) => chunk.push(m),
            None => break,
        }
        if chunk.len() >= 8192 {
            queue.push_slice(&chunk);
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        queue.push_slice(&chunk);
    }
    queue.close();
}

/// Resuelve la URL, construye el lector y la fuente, lanza el hilo de
/// decodificación y devuelve la fuente de cola para que el llamante la
/// asocie a un `Sink`. Termina la decodificación cuando la bandera `alive`
/// se pone a `false`.
fn iniciar_fuente(
    url: String,
    alive: Arc<AtomicBool>,
) -> Result<QueuedSource, Box<dyn std::error::Error>> {
    let client = build_client()?;
    let reader = RadioReader::resolve(&url, alive.clone(), &client, 0)?;
    // `RadioSource::new` decodifica el primer paquete, con lo que canales y tasa
    // ya son los reales del flujo al montar la cola y el remuestreador de rodio.
    let src = RadioSource::new(reader, alive.clone())?;
    let (queue, fuente) = AudioQueue::new(src.channels, src.sample_rate, alive.clone());
    let q = queue.clone();
    let a = alive.clone();
    std::thread::spawn(move || {
        // Si la decodificación paniquea, hay que cerrar la cola igualmente para
        // que el `Sink` termine en vez de quedarse emitiendo silencio.
        let q_cierre = q.clone();
        let _ = ejecutar_protegido("decodificacion", move || {
            bucle_decodificacion(src, q, a);
        });
        q_cierre.close();
    });
    Ok(fuente)
}

/// Lee los metadatos ICY ("now playing") del flujo de la emisora en paralelo al
/// reproductor. Muchas emisoras MP3/AAC (Shoutcast/Icecast) envían, cada
/// `icy-metaint` bytes de audio, un bloque con `StreamTitle='Artista - Canción'`.
/// Actualiza `meta` continuamente y se detiene cuando `alive` se pone a `false`
/// (por ejemplo, al cambiar de emisora o salir).
fn icy_metadata_loop(url: String, alive: Arc<AtomicBool>, meta: Arc<Mutex<String>>) {
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            log!("icy", "no se pudo crear el cliente: {e}");
            return;
        }
    };

    // Resolver la URL final tras posibles redirecciones.
    let final_url = match client.get(&url).send() {
        Ok(resp) if resp.status().is_success() => resp.url().to_string(),
        Ok(resp) => {
            log!("icy", "respuesta inesperada al resolver: {}", resp.status());
            return;
        }
        Err(e) => {
            log!("icy", "fallo de conexión para metadatos: {e}");
            return;
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("icy-metadata"),
        HeaderValue::from_static("1"),
    );

    let resp = match client.get(&final_url).headers(headers).send() {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            log!("icy", "metadatos: estado {}", r.status());
            return;
        }
        Err(e) => {
            log!("icy", "metadatos: error de petición: {e}");
            return;
        }
    };

    let metaint: usize = match resp.headers().get("icy-metaint") {
        Some(v) => match v.to_str().ok().and_then(|s| s.parse::<usize>().ok()) {
            Some(n) if n > 0 => n,
            _ => {
                log!("icy", "sin soporte de metadatos ICY en esta emisora");
                return;
            }
        },
        None => {
            log!("icy", "sin cabecera icy-metaint en esta emisora");
            return;
        }
    };

    let mut reader = resp;
    let mut audio_buf = vec![0u8; metaint];
    let mut len_buf = [0u8; 1];

    loop {
        if !alive.load(Ordering::Relaxed) {
            return;
        }

        // Descartar el bloque de audio de `metaint` bytes.
        let mut leidos = 0;
        while leidos < metaint {
            match reader.read(&mut audio_buf[leidos..]) {
                Ok(0) => {
                    log!("icy", "fin de flujo de metadatos");
                    return;
                }
                Ok(n) => leidos += n,
                Err(e) => {
                    log!("icy", "error de lectura de metadatos: {e}");
                    return;
                }
            }
        }

        if !alive.load(Ordering::Relaxed) {
            return;
        }

        // Longitud del bloque de metadatos (múltiplo de 16).
        if reader.read_exact(&mut len_buf).is_err() {
            return;
        }
        let mlen = len_buf[0] as usize * 16;
        if mlen == 0 {
            continue;
        }

        let mut mbuf = vec![0u8; mlen];
        if reader.read_exact(&mut mbuf).is_err() {
            return;
        }

        if let Some(title) = parse_stream_title(&mbuf) {
            let mut g = meta.candado();
            if *g != title {
                *g = title;
            }
        }
    }
}

/// Extrae `StreamTitle='...'` de un bloque de metadatos ICY.
fn parse_stream_title(buf: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(buf);
    let marker = "StreamTitle='";
    let start = s.find(marker)? + marker.len();
    let rest = &s[start..];
    let end = rest.find('\'')?;
    let title = rest[..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_sample_urls() {
        let urls = [
            "https://stream.radioparadise.com/mp3-192", // MP3
            "https://classicalking.streamguys1.com/king-fm-aac-128k", // AAC
            "https://rtvelivestream.rtve.es/rtvesec/rne/rne_r1_main.m3u8", // HLS (TS+AAC)
            "https://rnelivestream.rtve.es/rne1/val/128/seglist.m3u8", // HLS (ADTS AAC con ID3)
        ];
        for url in urls {
            let client = build_client().expect("client");
            let alive = Arc::new(AtomicBool::new(true));
            let reader = match RadioReader::resolve(url, alive.clone(), &client, 0) {
                Ok(r) => r,
                Err(e) => {
                    log!("test", "{url} -> resolve error: {e}");
                    continue;
                }
            };
            let mut src = match RadioSource::new(reader, alive.clone()) {
                Ok(s) => s,
                Err(e) => {
                    log!("test", "{url} -> probe error: {e}");
                    continue;
                }
            };
            let mut count = 0u64;
            for _ in 0..5000 {
                match src.next() {
                    Some(_) => count += 1,
                    None => break,
                }
                if count > 100_000 {
                    break;
                }
            }
            println!(
                "[test] {url} -> {count} samples, channels={}, rate={}",
                src.channels, src.sample_rate
            );
            assert!(count > 1000, "no se decodificaron muestras para {url}");
            alive.store(false, Ordering::Relaxed);
        }
    }

    /// Servidor HTTP local mínimo que responde 404 a cualquier petición y
    /// devuelve su URL base. Se usa para probar el manejo de URLs caídas sin
    /// depender de la red externa. Mantiene los sockets abiertos hasta el final
    /// para evitar que un cierre prematuro provoque un RST antes de leer el 404.
    fn servidor_404() -> String {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let mut mantenidos = Vec::new();
            for _ in 0..8 {
                if let Ok((mut sock, _)) = listener.accept() {
                    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf);
                    let _ = sock.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    let _ = sock.flush();
                    mantenidos.push(sock);
                }
            }
        });
        format!("http://{addr}")
    }

    /// Una emisora que responde 404 debe dar error al conectar en vez de dejar
    /// la interfaz en «Conectando…» para siempre.
    #[test]
    fn conexion_404_da_error_en_vez_de_colgarse() {
        let client = build_client().expect("client");
        let alive = Arc::new(AtomicBool::new(true));
        let url = format!("{}/emisora.mp3", servidor_404());
        let msg = match RadioReader::resolve(&url, alive.clone(), &client, 0) {
            Ok(_) => panic!("no debería haber conectado con un 404"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("404"), "mensaje inesperado: {msg}");
    }

    /// Una playlist HLS caída también debe dar error en vez de reintentar sin
    /// fin durante la carga inicial.
    #[test]
    fn playlist_hls_404_da_error() {
        let client = build_client().expect("client");
        let alive = Arc::new(AtomicBool::new(true));
        let url = format!("{}/emisora.m3u8", servidor_404());
        let msg = match RadioReader::resolve(&url, alive.clone(), &client, 0) {
            Ok(_) => panic!("no debería haber cargado la lista con un 404"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("lista"), "mensaje inesperado: {msg}");
    }

    /// Una emisora inalcanzable (conexión rechazada) agota los reintentos y
    /// devuelve error en lugar de reintentar sin fin.
    #[test]
    fn conexion_rechazada_agota_intentos() {
        let client = build_client().expect("client");
        let alive = Arc::new(AtomicBool::new(true));
        // Puerto sin servicio en localhost: conexión rechazada al instante.
        let url = "http://127.0.0.1:9/emisora.mp3";
        let msg = match RadioReader::resolve(url, alive.clone(), &client, 0) {
            Ok(_) => panic!("no debería haber conectado con un puerto cerrado"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("no se pudo conectar"),
            "mensaje inesperado: {msg}"
        );
    }

    /// Construye una trama ADTS sintética. `mpeg2` selecciona la sincronía
    /// MPEG-2 (`0xFF F9`) frente a MPEG-4 (`0xFF F1`); `etiqueta` rellena el
    /// payload para poder comprobar que no se pierde nada.
    fn trama_adts(mpeg2: bool, etiqueta: u8, relleno: usize) -> Vec<u8> {
        let len = 7 + relleno;
        let mut f = vec![0u8; len];
        f[0] = 0xFF;
        f[1] = if mpeg2 { 0xF9 } else { 0xF1 };
        f[2] = 0x50; // perfil LC (01), sf_idx 4 (44100), privado 0
        f[3] = 0x80 | ((len >> 11) as u8 & 0x03); // canal 2 en los bits altos
        f[4] = ((len >> 3) & 0xFF) as u8;
        f[5] = (((len & 0x07) as u8) << 5) | 0x1F;
        f[6] = 0xFC;
        for (i, b) in f[7..].iter_mut().enumerate() {
            *b = etiqueta.wrapping_add(i as u8);
        }
        f
    }

    /// Extrae todo lo disponible en el normalizador.
    fn drenar(n: &mut AdtsNormalizer) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let k = n.sacar(&mut buf);
            if k == 0 {
                break;
            }
            out.extend_from_slice(&buf[..k]);
        }
        out
    }

    /// El normalizador descarta la basura inicial (se conecta a mitad de trama)
    /// y convierte ADTS MPEG-2 (`0xFF F9`) en MPEG-4 (`0xFF F1`), que es el
    /// único que reconoce el demuxador ADTS de `symphonia`.
    #[test]
    fn adts_normaliza_mpeg2_y_descarta_basura() {
        let mut datos = vec![0xAA, 0xBB, 0xCC];
        for e in [0x10u8, 0x20, 0x30] {
            datos.extend(trama_adts(true, e, 200));
        }
        let mut n = AdtsNormalizer::new();
        n.push(&datos);

        let mut esperado = Vec::new();
        for e in [0x10u8, 0x20, 0x30] {
            let mut f = trama_adts(true, e, 200);
            f[1] = 0xF1; // la salida siempre es MPEG-4
            esperado.extend(f);
        }
        assert_eq!(drenar(&mut n), esperado);
    }

    /// Si el flujo no es ADTS (p. ej. MP3) se entrega intacto.
    #[test]
    fn adts_no_altera_flujos_ajenos() {
        let datos = vec![0x55u8; 40_000];
        let mut n = AdtsNormalizer::new();
        n.push(&datos);
        assert_eq!(drenar(&mut n), datos);
    }

    /// Las tramas que llegan partidas en trozos se reensamblan correctamente.
    #[test]
    fn adts_reensambla_tramas_partidas() {
        let f1 = trama_adts(true, 0x01, 300);
        let f2 = trama_adts(false, 0x02, 300);
        let mut datos = f1.clone();
        datos.extend(f2.clone());

        let mut n = AdtsNormalizer::new();
        for trozo in datos.chunks(5) {
            n.push(trozo);
        }

        let mut esperado = f1.clone();
        esperado[1] = 0xF1;
        let mut e2 = f2.clone();
        e2[1] = 0xF1;
        esperado.extend(e2);
        assert_eq!(drenar(&mut n), esperado);
    }

    /// Una sincronía casual dentro del payload no desalinea el encuadre.
    #[test]
    fn adts_ignora_falsas_sincronias_del_payload() {
        let mut f1 = trama_adts(true, 0, 100);
        // Se introduce por casualidad 0xFF 0xF1 0x50 0x80 en el payload.
        f1[7] = 0xFF;
        f1[8] = 0xF1;
        f1[9] = 0x50;
        f1[10] = 0x80;
        let f2 = trama_adts(true, 0x42, 100);

        let mut n = AdtsNormalizer::new();
        let mut datos = f1.clone();
        datos.extend(f2.clone());
        n.push(&datos);

        let mut esperado = f1.clone();
        esperado[1] = 0xF1;
        let mut e2 = f2.clone();
        e2[1] = 0xF1;
        esperado.extend(e2);
        assert_eq!(drenar(&mut n), esperado);
    }

    /// `adts_frame_len` rechaza sincronías de MP3 y frecuencias prohibidas.
    #[test]
    fn adts_rechaza_cabeceras_invalidas() {
        let f = trama_adts(false, 0, 10);
        assert_eq!(adts_frame_len(&f, 0), Some(f.len()));

        let mut malo = trama_adts(false, 0, 10);
        malo[2] = 0x3C; // sf_idx = 15 (prohibido)
        assert_eq!(adts_frame_len(&malo, 0), None);

        let mut mp3 = trama_adts(false, 0, 10);
        mp3[1] = 0xFB; // sincronía de MP3, no de ADTS
        assert_eq!(adts_frame_len(&mp3, 0), None);
    }

    /// Diagnóstico manual (no se ejecuta por defecto): intenta resolver y probar
    /// el formato de unas URLs concretas e imprime el resultado. Útil para
    /// depurar emisoras que no suenan:
    /// `cargo test -- --ignored diagnostico_formatos --nocapture`.
    /// El vigilante desactiva la bandera `alive` a los 15 s para que los
    /// reintentos de conexión/lectura no dejen el test colgado.
    #[test]
    #[ignore]
    fn diagnostico_formatos() {
        let urls = [
            "http://abc.streamguys1.com/live/newsradio/icecast.audio",
            "https://knkx-live-a.edge.audiocdn.com/6285_128k",
            "https://h3004255.stratoserver.net:8000/oldiesfmmovil",
            "http://relay.publicdomainradio.org:80/jazz_swing.aac",
        ];
        for url in urls {
            let client = build_client().expect("client");
            let alive = Arc::new(AtomicBool::new(true));
            let vigilante = alive.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(15));
                vigilante.store(false, Ordering::Relaxed);
            });
            match RadioReader::resolve(url, alive.clone(), &client, 0) {
                Ok(reader) => match RadioSource::new(reader, alive.clone()) {
                    Ok(mut src) => {
                        let mut count = 0u64;
                        for _ in 0..2000 {
                            if src.next().is_none() {
                                break;
                            }
                            count += 1;
                            if count > 50_000 {
                                break;
                            }
                        }
                        println!(
                            "[diag] {url} -> OK ch={} rate={} muestras={count}",
                            src.channels, src.sample_rate
                        );
                    }
                    Err(e) => println!("[diag] {url} -> probe error: {e}"),
                },
                Err(e) => println!("[diag] {url} -> resolve error: {e}"),
            }
            alive.store(false, Ordering::Relaxed);
        }
    }

    /// Cronometra el flujo HLS en vivo midiendo cuánto tarda cada `next()` del
    /// decodificador. Cualquier llamada mucho más lenta que el resto indica un
    /// bloqueo (descarga de segmento, espera en el borde del directo, etc.) que
    /// se traduce en el corte audible entre fragmentos.
    #[test]
    #[ignore]
    fn cronometra_hls() {
        let url = "https://rtvelivestream.rtve.es/rtvesec/rne/rne_r1_main.m3u8";
        let client = build_client().expect("client");
        let alive = Arc::new(AtomicBool::new(true));
        let reader = match RadioReader::resolve(url, alive.clone(), &client, 0) {
            Ok(r) => r,
            Err(e) => {
                log!("test", "resolve error: {e}");
                return;
            }
        };
        let mut src = match RadioSource::new(reader, alive.clone()) {
            Ok(s) => s,
            Err(e) => {
                log!("test", "probe error: {e}");
                return;
            }
        };

        let t0 = std::time::Instant::now();
        let mut total: u64 = 0;
        let mut lento_10ms = 0u32;
        let mut lento_50ms = 0u32;
        let mut lento_100ms = 0u32;
        let mut lento_500ms = 0u32;
        let mut pico: std::time::Duration = std::time::Duration::ZERO;
        let mut buckets: Vec<(std::time::Duration, u64)> = Vec::new();
        // cada segundo de audio registrar el wall-time que costó y qué pico de bloqueo hubo
        let rate = 48000u64;
        let mut next_sec = rate;
        let mut last_wall = t0;
        let mut pico_sec = std::time::Duration::ZERO;

        // Ritmo a tiempo real (como rodio): tras obtener la muestra `total`, se duerme
        // hasta la marca de tiempo que le correspondería al decodificar 48000/s.
        loop {
            let t = std::time::Instant::now();
            match src.next() {
                Some(_) => total += 1,
                None => break,
            }
            let el = t.elapsed();
            if el >= std::time::Duration::from_millis(10) {
                lento_10ms += 1;
                if el >= std::time::Duration::from_millis(50) {
                    lento_50ms += 1;
                    if el >= std::time::Duration::from_millis(100) {
                        lento_100ms += 1;
                        if el >= std::time::Duration::from_millis(500) {
                            lento_500ms += 1;
                        }
                    }
                }
            }
            if el > pico {
                pico = el;
            }
            if el > pico_sec {
                pico_sec = el;
            }
            let target = t0 + Duration::from_secs_f64((total as f64) / rate as f64);
            let now = std::time::Instant::now();
            if now < target {
                std::thread::sleep(target - now);
            }
            if total >= next_sec {
                let now = std::time::Instant::now();
                buckets.push((now - last_wall, pico_sec.as_millis() as u64));
                last_wall = now;
                pico_sec = std::time::Duration::ZERO;
                next_sec += rate;
                if buckets.len() >= 30 {
                    break;
                }
            }
            if std::time::Instant::now() - t0 > Duration::from_secs(120) {
                break;
            }
        }

        println!(
            "[crono] total={} muestras ({} s de audio), wall={:?}",
            total,
            total / rate,
            std::time::Instant::now() - t0
        );
        println!(
            "[crono] next() >10ms: {lento_10ms}, >50ms: {lento_50ms}, >100ms: {lento_100ms}, >500ms: {lento_500ms}, pico: {pico:?}"
        );
        for (i, (wall, pico_ms)) in buckets.iter().enumerate() {
            println!("[crono] segundo {i}: wall={wall:?} pico_bloqueo={pico_ms}ms");
        }
        alive.store(false, Ordering::Relaxed);
    }

    /// Al cerrarse por fin natural del flujo (VOD con `#EXT-X-ENDLIST`), la cola
    /// drena el audio pendiente antes de terminar.
    #[test]
    fn cola_drena_tras_cerrar() {
        let alive = Arc::new(AtomicBool::new(true));
        let (queue, _fuente) = AudioQueue::new(2, 48000, alive.clone());
        let block: Vec<f32> = vec![0.5; 512];
        queue.push_slice(&block);
        queue.close();
        let mut cuentas = 0;
        while queue.pop().is_some() {
            cuentas += 1;
        }
        assert_eq!(cuentas, 512);
        assert_eq!(queue.pop(), None);
    }

    /// Al cambiar de emisora (`alive` a `false`) el corte es inmediato: la cola
    /// devuelve `None` aunque aún quede audio pendiente (~3 s).
    #[test]
    fn cola_corta_de_inmediato_al_cambiar() {
        let alive = Arc::new(AtomicBool::new(true));
        let (queue, _fuente) = AudioQueue::new(2, 48000, alive.clone());
        let block: Vec<f32> = vec![1.0; 4096];
        queue.push_slice(&block);

        // Emisora viva: entrega lo que tiene.
        assert_eq!(queue.pop(), Some(1.0));

        // Cambio de emisora: corta ya, sin drenar lo pendiente.
        alive.store(false, Ordering::Relaxed);
        for _ in 0..100 {
            assert_eq!(queue.pop(), None);
        }
    }

    /// Regresión: un segmento HLS servido como flujo elemental (ADTS AAC) no
    /// debe demultiplexarse como si fuera MPEG-TS. Antes, un `0x47` casual en
    /// los datos se interpretaba como cabecera de paquete y la longitud de
    /// adaptación podía sacar el índice fuera del paquete, provocando un pánico
    /// de slicing que abortaba el proceso (emisoras HLS como RNE1 Valencia).
    #[test]
    fn demux_ts_no_entra_en_panico_con_datos_corruptos() {
        let mut st = TsState::new();
        let mut out = Vec::new();

        // Paquete con byte de sincronía, adaptación + payload y una longitud de
        // adaptación (200) que no cabe en los 188 bytes del paquete.
        let mut data = vec![0u8; 188 * 3];
        data[0] = 0x47;
        data[3] = 0x30; // afc = 3 (adaptación + payload)
        data[4] = 200; // longitud de adaptación > 183 → desbordaba el slicing
        data[188] = 0x47;
        data[191] = 0x30;
        data[192] = 250;
        data[376] = 0x47;
        demux_ts(&data, &mut st, &mut out);

        // Datos arbitrarios: no debe entrar en pánico pase lo que pase.
        let basura: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 0x47) as u8).collect();
        demux_ts(&basura, &mut st, &mut out);
    }

    /// `es_mpeg_ts` distingue un flujo MPEG-TS de uno elemental (p. ej. ADTS).
    #[test]
    fn detecta_mpeg_ts_frente_a_flujo_elemental() {
        // Tres paquetes alineados con byte de sincronía.
        let mut ts = vec![0u8; 188 * 3];
        ts[0] = 0x47;
        ts[188] = 0x47;
        ts[376] = 0x47;
        assert!(es_mpeg_ts(&ts));

        // Un único paquete también es TS.
        let mut uno = vec![0u8; 188];
        uno[0] = 0x47;
        assert!(es_mpeg_ts(&uno));

        // Flujo elemental que empieza por ID3 + ADTS: no es TS.
        let mut aac = vec![0u8; 4096];
        aac[..3].copy_from_slice(b"ID3");
        aac[1024] = 0xFF;
        aac[1025] = 0xF1;
        assert!(!es_mpeg_ts(&aac));

        // Un `0x47` suelto al principio pero sin continuidad tampoco es TS.
        let mut falso = vec![0u8; 188 * 2];
        falso[0] = 0x47;
        assert!(!es_mpeg_ts(&falso));

        // Segmentos demasiado cortos nunca son TS.
        assert!(!es_mpeg_ts(&[0x47, 0x00, 0x00]));
    }

    /// `tamano_id3` calcula el tamaño real de la etiqueta ID3v2 inicial.
    #[test]
    fn tamano_id3_lee_la_cabecera() {
        let mut etiqueta = vec![0u8; 1024];
        // "ID3" + versión 4.0 + flags 0 + tamaño synchsafe 1014.
        etiqueta[..3].copy_from_slice(b"ID3");
        etiqueta[3] = 4;
        let size = 1014u32;
        etiqueta[6] = (size >> 21) as u8 & 0x7F;
        etiqueta[7] = (size >> 14) as u8 & 0x7F;
        etiqueta[8] = (size >> 7) as u8 & 0x7F;
        etiqueta[9] = (size & 0x7F) as u8;
        assert_eq!(tamano_id3(&etiqueta), 1024);

        // Sin ID3.
        assert_eq!(tamano_id3(&[0xFF, 0xF1, 0x00, 0x00]), 0);
        assert_eq!(tamano_id3(b"ID3"), 0);
    }

    /// Un segmento elemental (ADTS AAC) se entrega tal cual, sin su ID3 inicial,
    /// para que quede un flujo ADTS continuo al concatenar segmentos.
    #[test]
    fn segmento_elemental_pasa_sin_id3() {
        let mut data = vec![0u8; 1024 + 3];
        data[..3].copy_from_slice(b"ID3");
        let size = 1014u32;
        data[6] = (size >> 21) as u8 & 0x7F;
        data[7] = (size >> 14) as u8 & 0x7F;
        data[8] = (size >> 7) as u8 & 0x7F;
        data[9] = (size & 0x7F) as u8;
        data[1024] = 0xFF;
        data[1025] = 0xF1;
        data[1026] = 0x50;

        let mut st = TsState::new();
        let mut out = Vec::new();
        demux_segmento(&data, &mut st, &mut out);
        assert_eq!(out, &data[1024..]);
    }

    /// Como `cronometra_hls`, pero consume de la `AudioQueue` (el camino real de
    /// `iniciar`). Valida que la cola absorbe las esperas de red/fronteras: las
    /// muestras devueltas como silencio (cola vacía) deben ser ~0 y el `next()`
    /// nunca debe bloquear el hilo de audio.
    #[test]
    #[ignore]
    fn cronometra_cola() {
        use std::time::Instant;

        let url = "https://rtvelivestream.rtve.es/rtvesec/rne/rne_r1_main.m3u8";
        let client = build_client().expect("client");
        let alive = Arc::new(AtomicBool::new(true));

        let reader = match RadioReader::resolve(url, alive.clone(), &client, 0) {
            Ok(r) => r,
            Err(e) => {
                log!("test", "resolve error: {e}");
                return;
            }
        };
        let src = match RadioSource::new(reader, alive.clone()) {
            Ok(s) => s,
            Err(e) => {
                log!("test", "probe error: {e}");
                return;
            }
        };
        let (queue, mut fuente) = AudioQueue::new(src.channels, src.sample_rate, alive.clone());
        let q = queue.clone();
        let a = alive.clone();
        std::thread::spawn(move || bucle_decodificacion(src, q, a));

        let rate = 48000u64;
        let t0 = Instant::now();
        let mut total: u64 = 0;
        let mut silencios: u64 = 0;
        let mut lento: u32 = 0;
        let mut pico = Duration::ZERO;
        loop {
            let t = Instant::now();
            let m = fuente.next();
            let el = t.elapsed();
            if el > pico {
                pico = el;
            }
            if el >= Duration::from_millis(5) {
                lento += 1;
            }
            match m {
                Some(v) => {
                    if v == 0.0 {
                        silencios += 1;
                    }
                    total += 1;
                }
                None => break,
            }
            let target = t0 + Duration::from_secs_f64((total as f64) / rate as f64);
            let now = Instant::now();
            if now < target {
                std::thread::sleep(target - now);
            }
            if total >= rate * 30 {
                break;
            }
            if Instant::now() - t0 > Duration::from_secs(90) {
                break;
            }
        }
        let wall = Instant::now() - t0;
        let por_mil = (silencios * 1000).checked_div(total).unwrap_or(0);
        println!(
            "[crono_cola] total={} muestras ({} s), wall={wall:?}",
            total,
            total / rate
        );
        println!(
            "[crono_cola] silencios(cola vacía)={silencios} ({por_mil}‰ de <<{} s>>), lento>5ms={lento}, pico={pico:?}",
            silencios / rate
        );
        alive.store(false, Ordering::Relaxed);
        queue.close();
    }
}
