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
//! # radio-gui-rust
//!
//! Programa para oír emisoras de internet con interfaz gráfica (eframe/egui).
//! El audio se reproduce de forma nativa en Rust (`rodio` + `symphonia` +
//! `reqwest`), sin depender de ningún reproductor externo, tal y como se define
//! en `audio.rs`.
//!
//! Funciones principales: lista de emisoras, modo noticias automático,
//! temporizador de apagado, atajos de teclado y persistencia de la última
//! emisora escuchada.
//!

mod audio;

use audio::{EstadoReproduccion, Player};
use chrono::{Local, Timelike};
use eframe::egui;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> eframe::Result {
    let mut estado = Radio::load();
    estado.reproducir_actual();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([VENTANA_ANCHO, VENTANA_ALTO])
            .with_min_inner_size([VENTANA_MIN_ANCHO, VENTANA_MIN_ALTO]),
        // Recuerda tamaño y posición de la ventana entre ejecuciones. El estado
        // se guarda junto al ejecutable (misma carpeta que la configuración),
        // de modo que es portable si se copia la carpeta.
        persist_window: true,
        persistence_path: Some(Radio::get_base_path().join("radio_ui.ron")),
        ..Default::default()
    };

    eframe::run_native(
        "Radio GUI - Rust",
        options,
        Box::new(|_cc| Ok(Box::new(estado))),
    )
}

/// Tamaño inicial de la ventana: pensado para que la lista de emisoras muestre
/// al menos 16 estaciones sin desplazar.
const VENTANA_ANCHO: f32 = 700.0;
const VENTANA_ALTO: f32 = 500.0;
/// Tamaño mínimo permitido; la lista pasa a desplazarse si no caben todas.
const VENTANA_MIN_ANCHO: f32 = 520.0;
const VENTANA_MIN_ALTO: f32 = 320.0;
/// Ancho inicial del panel lateral de emisoras.
const PANEL_LISTA_ANCHO: f32 = 240.0;

struct Emisora {
    nombre: String,
    url: String,
}

#[derive(PartialEq)]
enum ModoReproduccion {
    Normal,
    EsperandoNoticias,
    EnNoticias,
}

struct Radio {
    lista_emisoras: Vec<Emisora>,
    noticias_url: String,
    inicio_noticias: u32,
    fin_noticias: u32,
    ultima_estacion: usize,
    player: Option<Player>,
    modo: ModoReproduccion,
    minutos_temporizador: u32,
    instante_final: Option<Instant>,
    meta_scroll: f32,
    meta_pausa: f32,
    /// Avisos de arranque (configuración o audio). Si hay alguno se muestra en
    /// la ventana, en vez de arrancar en silencio como ocurría antes.
    avisos: Vec<String>,
    /// Volumen efectivo aplicado al reproductor (0.0–2.0).
    volumen: f32,
    /// Indica si el audio está pausado.
    pausado: bool,
    /// Título dinámico de la ventana, basado en los metadatos ICY.
    titulo_ventana: String,
    /// Ancho medido (en el frame anterior) de cada fila de controles, para
    /// centrarla horizontalmente al redimensionar la ventana.
    ancho_fila_nav: f32,
    ancho_fila_control: f32,
    ancho_fila_timer: f32,
}

/// Parsea un fichero M3U de emisoras: el nombre se toma de la línea
/// `#EXTINF:...,` previa a cada URL.
fn parse_emisoras(contenido: &str) -> Vec<Emisora> {
    let mut lista = Vec::new();
    let mut nombre_temp = String::from("Desconocida");

    for linea in contenido.lines() {
        let l = linea.trim();
        if l.starts_with("#EXTINF:") {
            if let Some((_, n)) = l.rsplit_once(',') {
                nombre_temp = n.to_string();
            }
        } else if !l.is_empty() && !l.starts_with('#') {
            lista.push(Emisora {
                nombre: nombre_temp.clone(),
                url: l.to_string(),
            });
            nombre_temp = String::from("Desconocida");
        }
    }

    lista
}

/// Altura estimada del bloque de controles inferior (tres filas + separaciones).
const ALTO_CONTROLES: f32 = 3.0 * 30.0 + 8.0 + 10.0 + 12.0;

/// Hueco a anteponer para centrar horizontalmente un contenido de ancho
/// `ancho` dentro de `disponible`. Si el contenido no cabe, devuelve 0 (se
/// alinea a la izquierda).
fn hueco_centrado(disponible: f32, ancho: f32) -> f32 {
    ((disponible - ancho) * 0.5).max(0.0)
}

/// Coordenada Y donde empiezan los controles inferiores: se anclan al fondo del
/// panel (`y_fondo - ALTO_CONTROLES`) cuando sobra altura, y si no caben se
/// colocan justo debajo del contenido superior (`y_superior`).
fn inicio_controles(y_superior: f32, y_fondo: f32) -> f32 {
    y_superior.max(y_fondo - ALTO_CONTROLES)
}

/// Dibuja una fila horizontal de controles centrada en el ancho disponible.
///
/// `egui` no centra por sí solo filas cuyo ancho no se conoce de antemano, así
/// que se usa el ancho medido en el frame anterior (`ancho_previo`): se antepone
/// el hueco necesario para que la fila quede centrada. Devuelve el ancho medido
/// en este frame, que se guarda para el siguiente. En el primer frame (o tras
/// redimensionar) la fila puede aparecer sin centrar un instante y se corrige de
/// inmediato en el frame siguiente.
fn fila_centrada<R>(
    ui: &mut egui::Ui,
    ancho_previo: f32,
    contenido: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, f32) {
    let disponible = ui.available_width();
    let hueco = hueco_centrado(disponible, ancho_previo);
    let mut ancho = 0.0_f32;
    let respuesta = ui.horizontal(|ui| {
        if hueco > 0.0 {
            ui.add_space(hueco);
        }
        let valor = contenido(ui);
        // El rectángulo usado incluye el hueco inicial: se descuenta.
        ancho = (ui.min_rect().width() - hueco).max(0.0);
        valor
    });
    (respuesta.inner, ancho)
}

/// Indica si `minuto` (0-59) cae dentro de la franja de noticias `inicio`-`fin`.
/// Soporta franjas que cruzan la medianoche (p. ej. `22` -> `6`). Una franja con
/// inicio igual a fin se considera desactivada.
fn en_franja_noticias(minuto: u32, inicio: u32, fin: u32) -> bool {
    if inicio == fin {
        return false;
    }
    if inicio < fin {
        minuto >= inicio && minuto < fin
    } else {
        // Cruza medianoche: [inicio, 59] ∪ [0, fin).
        minuto >= inicio || minuto < fin
    }
}

/// Texto y color con que la GUI representa cada estado de reproducción.
fn estado_visual(estado: &EstadoReproduccion) -> (String, egui::Color32) {
    match estado {
        EstadoReproduccion::Detenido => ("⏹ Detenido".to_string(), egui::Color32::GRAY),
        EstadoReproduccion::Conectando => (
            "⏳ Conectando…".to_string(),
            egui::Color32::from_rgb(235, 190, 20),
        ),
        EstadoReproduccion::Reproduciendo => (
            "🎵 Reproduciendo".to_string(),
            egui::Color32::LIGHT_GREEN,
        ),
        EstadoReproduccion::Error(m) => (
            format!("⚠️ Error: {m}"),
            egui::Color32::from_rgb(235, 90, 90),
        ),
    }
}

impl Radio {
    fn get_base_path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Carga la configuración de forma tolerante a fallos. Un fichero que falte
    /// o no se pueda leer ya no impide arrancar: se registra como aviso visible
    /// en la ventana y se usan valores por defecto. Devuelve siempre un `Radio`
    /// utilizable.
    fn load() -> Self {
        let base = Self::get_base_path();
        let mut avisos: Vec<String> = Vec::new();

        let lista_emisoras = match fs::read_to_string(base.join("emisoras.m3u")) {
            Ok(contenido) => parse_emisoras(&contenido),
            Err(e) => {
                avisos.push(format!("No se pudo leer emisoras.m3u: {e}"));
                Vec::new()
            }
        };
        if lista_emisoras.is_empty() && avisos.is_empty() {
            avisos.push("La lista de emisoras está vacía.".to_string());
        }

        let noticias_url = match fs::read_to_string(base.join("noticias.m3u")) {
            Ok(contenido) => contenido
                .lines()
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .unwrap_or_default()
                .to_string(),
            Err(e) => {
                avisos.push(format!(
                    "No se pudo leer noticias.m3u (modo noticias desactivado): {e}"
                ));
                String::new()
            }
        };

        let (inicio_noticias, fin_noticias) =
            match fs::read_to_string(base.join("minutos_noticias.txt")) {
                Ok(tiempos) => {
                    let mut lineas = tiempos.lines();
                    let inicio = lineas.next().unwrap_or("0").trim().parse().unwrap_or(0);
                    let fin = lineas.next().unwrap_or("0").trim().parse().unwrap_or(0);
                    (inicio, fin)
                }
                Err(e) => {
                    avisos.push(format!(
                        "No se pudo leer minutos_noticias.txt (franja 0-0 por defecto): {e}"
                    ));
                    (0, 0)
                }
            };

        let ultima = fs::read_to_string(base.join("ultima_estacion.txt")).or_else(|_| fs::read_to_string(base.join("última_estación.txt")))
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);
        // Clampear el índice persistido a la lista realmente cargada, para que
        // una lista editada no deje el reproductor apuntando fuera de rango.
        let ultima_estacion = if lista_emisoras.is_empty() {
            0
        } else {
            ultima.min(lista_emisoras.len() - 1)
        };

        let player = match Player::new() {
            Ok(p) => Some(p),
            Err(e) => {
                avisos.push(format!(
                    "No se pudo inicializar el audio (no habrá sonido): {e}"
                ));
                None
            }
        };

        Self {
            lista_emisoras,
            noticias_url,
            inicio_noticias,
            fin_noticias,
            ultima_estacion,
            player,
            modo: ModoReproduccion::Normal,
            minutos_temporizador: 0,
            instante_final: None,
            meta_scroll: 0.0,
            meta_pausa: 0.0,
            avisos,
            volumen: 1.0,
            pausado: false,
            titulo_ventana: "Radio GUI - Rust".to_string(),
            ancho_fila_nav: 0.0,
            ancho_fila_control: 0.0,
            ancho_fila_timer: 0.0,
        }
    }

    fn reproducir_url(&mut self, url: &str) {
        if let Some(p) = self.player.as_mut() {
            p.play(url.to_string());
        }
    }

    fn reproducir_actual(&mut self) {
        if let Some(e) = self.lista_emisoras.get(self.ultima_estacion) {
            let url = e.url.clone();
            self.reproducir_url(&url);
        }
    }

    fn detener_player(&mut self) {
        if let Some(p) = self.player.as_mut() {
            p.stop();
        }
    }

    fn guardar_indice(&self) {
        let base = Self::get_base_path();
        let _ = fs::write(
            base.join("ultima_estacion.txt"),
            self.ultima_estacion.to_string(),
        );
    }

    fn toggle_noticias(&mut self) {
        if self.modo == ModoReproduccion::Normal {
            self.modo = ModoReproduccion::EsperandoNoticias;
        } else {
            if self.modo == ModoReproduccion::EnNoticias {
                self.reproducir_actual();
            }
            self.modo = ModoReproduccion::Normal;
        }
    }

    fn gestionar_temporizador(&mut self) {
        if self.minutos_temporizador == 0 {
            self.minutos_temporizador = 90;
        } else if self.minutos_temporizador > 10 {
            self.minutos_temporizador -= 10;
        } else {
            self.minutos_temporizador = 0;
        }

        self.instante_final = if self.minutos_temporizador > 0 {
            Some(Instant::now() + Duration::from_secs((self.minutos_temporizador * 60) as u64))
        } else {
            None
        };
    }

    fn siguiente(&mut self) {
        if !self.lista_emisoras.is_empty() {
            self.ultima_estacion = (self.ultima_estacion + 1) % self.lista_emisoras.len();
            self.guardar_indice();
            self.reproducir_actual();
        }
    }

    fn anterior(&mut self) {
        if !self.lista_emisoras.is_empty() {
            self.ultima_estacion = if self.ultima_estacion == 0 {
                self.lista_emisoras.len() - 1
            } else {
                self.ultima_estacion - 1
            };
            self.guardar_indice();
            self.reproducir_actual();
        }
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        self.detener_player();
    }
}

impl eframe::App for Radio {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.dibujar(ctx);
    }
}

impl Radio {
    /// Dibuja toda la interfaz. Se separa de `update` para poder ejecutarla en
    /// pruebas sin necesidad de un `eframe::Frame` real.
    fn dibujar(&mut self, ctx: &egui::Context) {
        let minuto_actual = Local::now().minute();

        // Lógica automática de noticias: por franja horaria (no por igualdad
        // exacta de minuto), de modo que también entra si se activa el modo
        // estando ya dentro de la franja, y soporta el cruce de medianoche.
        if !self.noticias_url.is_empty() {
            let en_franja =
                en_franja_noticias(minuto_actual, self.inicio_noticias, self.fin_noticias);
            match self.modo {
                ModoReproduccion::EsperandoNoticias if en_franja => {
                    let url = self.noticias_url.clone();
                    self.reproducir_url(&url);
                    self.modo = ModoReproduccion::EnNoticias;
                }
                ModoReproduccion::EnNoticias if !en_franja => {
                    self.reproducir_actual();
                    self.modo = ModoReproduccion::EsperandoNoticias;
                }
                _ => {}
            }
        }

        // Lógica Temporizador Cuenta Atrás
        if let Some(fin) = self.instante_final {
            let ahora = Instant::now();
            if ahora >= fin {
                self.minutos_temporizador = 0;
                self.instante_final = None;
                self.guardar_indice();
                self.detener_player();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                let segundos_restantes = fin.duration_since(ahora).as_secs();
                self.minutos_temporizador = segundos_restantes.div_ceil(60) as u32;
            }
        }

        // --- LÓGICA DE TECLADO ---
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.guardar_indice();
            self.detener_player();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::A)) {
            self.anterior();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::S)) {
            self.siguiente();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::N)) {
            self.toggle_noticias();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::T)) {
            self.gestionar_temporizador();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.pausado = !self.pausado;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::L)) {
            // Ya no se usa el toggle de lista; la lista es permanente.
        }
        if ctx.input(|i| i.key_pressed(egui::Key::M)) {
            self.volumen = if self.volumen <= 0.001 { 1.0 } else { 0.0 };
        }

        ctx.request_repaint_after(Duration::from_secs(1));

        // Sincroniza volumen/pausa entre UI y Player y recoge el `Sink` pendiente
        // (puede llegar después de `play()`, p. ej. en HLS o redes lentas).
        if let Some(p) = self.player.as_mut() {
            p.poll();
            if (p.get_volumen() - self.volumen).abs() > f32::EPSILON {
                p.set_volumen(self.volumen);
            }
            if self.pausado && !p.is_pausado() {
                p.pausar();
            } else if !self.pausado && p.is_pausado() {
                p.reanudar();
            }
        }

        // Mientras se conecta conviene refrescar más a menudo para cosechar el
        // `Sink` en cuanto esté listo y que el audio arranque sin demora.
        let conectando = self
            .player
            .as_ref()
            .map(|p| matches!(p.get_estado(), EstadoReproduccion::Conectando))
            .unwrap_or(false);
        if conectando {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        // Actualiza el título de la ventana según los metadatos ICY.
        let meta_actual = self
            .player
            .as_ref()
            .map(|p| p.get_metadata())
            .unwrap_or_default();
        let nuevo_titulo = if meta_actual.trim().is_empty() {
            "Radio GUI - Rust".to_string()
        } else {
            format!("{} - Radio GUI - Rust", meta_actual.trim())
        };
        if nuevo_titulo != self.titulo_ventana {
            self.titulo_ventana = nuevo_titulo.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.titulo_ventana.clone()));
        }

        // Panel lateral con la lista de emisoras (siempre visible a la derecha).
        if !self.lista_emisoras.is_empty() {
            let mut seleccion: Option<usize> = None;
            egui::SidePanel::right("lista_emisoras")
                .resizable(true)
                .default_width(PANEL_LISTA_ANCHO)
                .min_width(180.0)
                .max_width(420.0)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("Emisoras");
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (idx, emisora) in self.lista_emisoras.iter().enumerate() {
                                let es_actual = idx == self.ultima_estacion;
                                let label = if es_actual {
                                    egui::RichText::new(format!("▶ {}", emisora.nombre)).strong()
                                } else {
                                    egui::RichText::new(&emisora.nombre)
                                };
                                if ui
                                    .selectable_label(es_actual, label)
                                    .on_hover_text(&emisora.url)
                                    .clicked()
                                {
                                    seleccion = Some(idx);
                                }
                            }
                        });
                });
            if let Some(idx) = seleccion {
                self.ultima_estacion = idx;
                self.guardar_indice();
                self.reproducir_actual();
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let rect = ui.available_rect_before_wrap();

            // Banda de avisos de arranque (si los hay): se dibuja arriba en rojo
            // y desplaza el resto del contenido hacia abajo.
            let n_avisos = self.avisos.len().min(2);
            let extra_avisos = if n_avisos > 0 {
                n_avisos as f32 * 18.0 + 6.0
            } else {
                0.0
            };
            if n_avisos > 0 {
                let rojo = egui::Color32::from_rgb(235, 90, 90);
                let mut y = rect.top() + 6.0;
                for aviso in self.avisos.iter().take(n_avisos) {
                    let galley = ui.painter().layout_no_wrap(
                        format!("⚠️ {aviso}"),
                        egui::FontId::proportional(12.0),
                        rojo,
                    );
                    let x = (rect.center().x - galley.size().x / 2.0).max(rect.min.x + 4.0);
                    ui.painter().galley(egui::pos2(x, y), galley, rojo);
                    y += 18.0;
                }
            }

            let row_1_3 = rect.top() + extra_avisos + (rect.height() * 0.165);
            let tam_base = 24.0;

            // 1. Dibujar Emisora (con contador si hay lista)
            let total_emisoras = self.lista_emisoras.len();
            let nombre_base = self
                .lista_emisoras
                .get(self.ultima_estacion)
                .map(|e| e.nombre.as_str())
                .unwrap_or("Radio");
            let nombre_mostrar = if total_emisoras == 0 {
                nombre_base.to_string()
            } else {
                format!(
                    "{} ({}/{})",
                    nombre_base,
                    self.ultima_estacion + 1,
                    total_emisoras
                )
            };
            let galley_nom = ui.painter().layout_no_wrap(
                nombre_mostrar,
                egui::FontId::proportional(tam_base),
                ui.visuals().strong_text_color(),
            );

            let row_info = row_1_3 + galley_nom.size().y + 6.0;

            ui.painter().galley(
                egui::pos2(rect.center().x - (galley_nom.size().x / 2.0), row_1_3),
                galley_nom,
                ui.visuals().strong_text_color(),
            );

            // --- 2. INFORMACIÓN DINÁMICA CON SCROLL + ESTADO DE REPRODUCCIÓN ---
            let url_streaming = self
                .lista_emisoras
                .get(self.ultima_estacion)
                .map(|e| e.url.as_str())
                .unwrap_or("Sin URL disponible");

            let (estado_txt, estado_col, es_error) = match self.player.as_ref() {
                Some(p) => {
                    let estado = p.get_estado();
                    let (txt, col) = estado_visual(&estado);
                    (txt, col, matches!(estado, EstadoReproduccion::Error(_)))
                }
                None => (
                    "⚠️ Audio no disponible".to_string(),
                    egui::Color32::from_rgb(235, 90, 90),
                    true,
                ),
            };

            let texto_informativo = format!("{} | Rust Backend", url_streaming);

            let tam_info = tam_base * 0.60;
            let alto_linea = tam_info * 1.6;

            let info_rect = egui::Rect::from_min_max(
                egui::pos2(rect.min.x + 15.0, row_info),
                egui::pos2(rect.max.x - 15.0, row_info + alto_linea),
            );

            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(info_rect)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
                |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("scroll_info_radio")
                        .auto_shrink([false; 2])
                        .show(ui, |ui| {
                            let galley_info = ui.painter().layout_no_wrap(
                                texto_informativo,
                                egui::FontId::proportional(tam_info),
                                ui.visuals().text_color(),
                            );

                            ui.expand_to_include_x(ui.max_rect().min.x + galley_info.size().x);

                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(galley_info.text().to_string())
                                        .font(egui::FontId::proportional(tam_info))
                                        .color(ui.visuals().text_color()),
                                )
                                .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                },
            );

            // --- 2b. METADATOS EN VIVO DE LA EMISORA (ICY) CON SCROLL LATERAL ---
            let row_meta = row_info + alto_linea;

            let meta_vivo = self
                .player
                .as_ref()
                .map(|p| p.get_metadata())
                .unwrap_or_default();

            let texto_meta = if meta_vivo.is_empty() {
                "📋 Esperando metadatos de la emisora…".to_string()
            } else {
                format!("📋 {}", meta_vivo)
            };

            let meta_rect = egui::Rect::from_min_max(
                egui::pos2(rect.min.x + 15.0, row_meta),
                egui::pos2(rect.max.x - 15.0, row_meta + alto_linea),
            );

            let dt = ctx.input(|i| i.stable_dt);
            let galley_meta = ui.painter().layout_no_wrap(
                texto_meta,
                egui::FontId::proportional(tam_info),
                egui::Color32::LIGHT_BLUE,
            );
            let text_w = galley_meta.size().x;
            let avail_w = meta_rect.width();

            // Pintamos recortado a meta_rect para que el texto no invada el resto.
            let painter = ui.painter_at(meta_rect);
            let y_meta = meta_rect.min.y + (meta_rect.height() - galley_meta.size().y) / 2.0;

            if text_w <= avail_w {
                // El texto cabe: se muestra centrado y sin scroll.
                self.meta_scroll = 0.0;
                self.meta_pausa = 0.0;
                let x = meta_rect.center().x - text_w / 2.0;
                painter.galley(
                    egui::pos2(x, y_meta),
                    galley_meta,
                    egui::Color32::LIGHT_BLUE,
                );
            } else {
                // El texto no cabe: pausa inicial de 1.5 s y luego scroll de
                // derecha a izquierda, repitiéndose el ciclo.
                let velocidad = 40.0; // píxeles por segundo
                let pausa_inicial = 1.5; // segundos estático antes de cada ciclo

                if self.meta_scroll <= 0.0 {
                    // Fase de pausa: el texto queda estático al principio.
                    self.meta_pausa += dt;
                    if self.meta_pausa >= pausa_inicial {
                        self.meta_scroll += velocidad * dt;
                    }
                } else {
                    self.meta_scroll += velocidad * dt;
                    let total = text_w + 40.0; // recorrido + hueco antes de reiniciar
                    if self.meta_scroll > total {
                        self.meta_scroll = 0.0;
                        self.meta_pausa = 0.0;
                    }
                }

                let x = meta_rect.min.x - self.meta_scroll;
                painter.galley(
                    egui::pos2(x, y_meta),
                    galley_meta,
                    egui::Color32::LIGHT_BLUE,
                );
                // Repintado continuo para animar el scroll aunque la ventana no cambie.
                ctx.request_repaint();
            }

            // --- 3. ESTADO / AVISO DESTACADO ---
            // Se dibuja centrado (horizontal y verticalmente) en el hueco que
            // queda entre los metadatos y los controles de volumen y salida, de
            // modo que los avisos (404, sin conexión, formato no reconocido,
            // lista no descargable…) se vean siempre de forma explícita.
            let row_botones = inicio_controles(row_meta + alto_linea + 15.0, rect.max.y);

            let zona_aviso = egui::Rect::from_min_max(
                egui::pos2(rect.min.x + 15.0, row_meta + alto_linea),
                egui::pos2(rect.max.x - 15.0, row_botones),
            );
            // Los errores se muestran más grandes y con caja para que destaquen;
            // los estados normales van en un tamaño discreto.
            let tam_aviso = if es_error { 20.0 } else { 15.0 };
            let galley_aviso = ui.painter().layout(
                estado_txt.clone(),
                egui::FontId::proportional(tam_aviso),
                estado_col,
                zona_aviso.width().max(50.0),
            );
            if zona_aviso.height() >= galley_aviso.size().y {
                let centro = zona_aviso.center();
                let pos = egui::pos2(
                    (centro.x - galley_aviso.size().x / 2.0).max(zona_aviso.min.x),
                    centro.y - galley_aviso.size().y / 2.0,
                );
                if es_error {
                    let relleno = egui::vec2(12.0, 7.0);
                    let caja = egui::Rect::from_min_size(
                        pos - relleno,
                        galley_aviso.size() + relleno * 2.0,
                    );
                    ui.painter()
                        .rect_filled(caja, 8.0, egui::Color32::from_rgb(58, 18, 18));
                    ui.painter().rect_stroke(
                        caja,
                        8.0,
                        egui::Stroke::new(1.5_f32, estado_col),
                        egui::StrokeKind::Outside,
                    );
                }
                ui.painter().galley(pos, galley_aviso, estado_col);
            }

            // --- 4. BOTONES Y CONTROLES INFERIORES ---
            // Se anclan al fondo de la ventana cuando sobra altura, de modo que
            // la información quede arriba y los controles abajo; si la ventana es
            // baja, se colocan justo debajo de los metadatos.

            ui.scope_builder(
                egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
                    egui::pos2(rect.min.x, row_botones),
                    egui::pos2(rect.max.x, rect.max.y),
                )),
                |ui| {
                    // Fila 1: navegación entre emisoras.
                    let (_, ancho_nav) = fila_centrada(ui, self.ancho_fila_nav, |ui| {
                        // Botón Anterior
                        if ui
                            .add(egui::Button::new(egui::RichText::new("⏮").size(20.0)))
                            .on_hover_text("Emisora anterior (A)")
                            .clicked()
                        {
                            self.anterior();
                        }

                        ui.add_space(8.0);

                        // Botón de Noticias
                        let texto_rt = match self.modo {
                            ModoReproduccion::Normal => egui::RichText::new("📰 Noticias")
                                .color(ui.visuals().text_color()),
                            ModoReproduccion::EsperandoNoticias => {
                                egui::RichText::new("⏳ Esperando noticias")
                                    .color(egui::Color32::from_rgb(235, 190, 20))
                            }
                            ModoReproduccion::EnNoticias => egui::RichText::new("📻 En noticias")
                                .color(egui::Color32::YELLOW),
                        };

                        if ui
                            .add(egui::Button::new(texto_rt.size(15.0)))
                            .on_hover_text("Activar / desactivar modo noticias (N)")
                            .clicked()
                        {
                            self.toggle_noticias();
                        }

                        ui.add_space(8.0);

                        // Botón Siguiente
                        if ui
                            .add(egui::Button::new(egui::RichText::new("⏭").size(20.0)))
                            .on_hover_text("Siguiente emisora (S)")
                            .clicked()
                        {
                            self.siguiente();
                        }
                    });
                    self.ancho_fila_nav = ancho_nav;

                    ui.add_space(8.0);

                    // Fila 2: reproducción (pausa, volumen) y salida.
                    let (_, ancho_control) = fila_centrada(ui, self.ancho_fila_control, |ui| {
                        // Botón Pausa / Reanudar
                        let icono_pausa = if self.pausado { "▶" } else { "⏸" };
                        if ui
                            .add(egui::Button::new(egui::RichText::new(icono_pausa).size(18.0)))
                            .on_hover_text("Pausar / Reanudar (espacio)")
                            .clicked()
                        {
                            self.pausado = !self.pausado;
                        }

                        ui.add_space(6.0);

                        // Botón Silenciar
                        let silenciado = self.volumen <= 0.001;
                        let icono_vol = if silenciado { "🔇" } else { "🔊" };
                        if ui
                            .add(egui::Button::new(egui::RichText::new(icono_vol).size(18.0)))
                            .on_hover_text("Silenciar / activar sonido (M)")
                            .clicked()
                        {
                            self.volumen = if silenciado { 1.0 } else { 0.0 };
                        }

                        // Deslizador de volumen
                        ui.add_sized(
                            [110.0, 18.0],
                            egui::Slider::new(&mut self.volumen, 0.0..=2.0).show_value(false),
                        )
                        .on_hover_text(format!("Volumen: {:.0}%", self.volumen * 100.0));

                        ui.add_space(14.0);

                        // Botón Salir
                        if ui
                            .add(egui::Button::new(egui::RichText::new("🛑 Salir").size(15.0)))
                            .on_hover_text("Salir de la aplicación (ESC)")
                            .clicked()
                        {
                            self.guardar_indice();
                            self.detener_player();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    self.ancho_fila_control = ancho_control;

                    ui.add_space(10.0);

                    // Fila 3: temporizador de apagado.
                    let (_, ancho_timer) = fila_centrada(ui, self.ancho_fila_timer, |ui| {
                        let texto_timer = if self.minutos_temporizador > 0 {
                            egui::RichText::new(format!(
                                "⏱ Apagado en: {} min",
                                self.minutos_temporizador
                            ))
                            .color(egui::Color32::LIGHT_BLUE)
                        } else {
                            egui::RichText::new("⏱ Temporizador desactivado")
                                .color(ui.visuals().text_color())
                        };

                        if ui
                            .add(egui::Button::new(texto_timer.size(14.0)))
                            .on_hover_text("Temporizador de apagado (T): 15, 30, 60, 120 min")
                            .clicked()
                        {
                            self.gestionar_temporizador();
                        }
                    });
                    self.ancho_fila_timer = ancho_timer;
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Radio mínimo, sin dispositivo de audio, para probar la interfaz.
    fn radio_de_prueba(emisoras: usize) -> Radio {
        Radio {
            lista_emisoras: (0..emisoras)
                .map(|i| Emisora {
                    nombre: format!("Emisora {i}"),
                    url: format!("http://ejemplo/{i}"),
                })
                .collect(),
            noticias_url: String::new(),
            inicio_noticias: 0,
            fin_noticias: 0,
            ultima_estacion: 0,
            player: None,
            modo: ModoReproduccion::Normal,
            minutos_temporizador: 0,
            instante_final: None,
            meta_scroll: 0.0,
            meta_pausa: 0.0,
            avisos: Vec::new(),
            volumen: 1.0,
            pausado: false,
            titulo_ventana: String::new(),
            ancho_fila_nav: 0.0,
            ancho_fila_control: 0.0,
            ancho_fila_timer: 0.0,
        }
    }

    /// Textos pintados con su rectángulo, recorriendo el árbol de formas.
    fn textos_pintados(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Rect)> {
        fn recoger(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(t) => out.push((
                    t.galley.text().to_string(),
                    egui::Rect::from_min_size(t.pos, t.galley.size()),
                )),
                egui::Shape::Vec(v) => {
                    for s in v {
                        recoger(s, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for cs in shapes {
            recoger(&cs.shape, &mut out);
        }
        out
    }

    /// Ejecuta varios frames de la interfaz sobre un `Radio` de prueba y
    /// devuelve los textos pintados con su rectángulo. El primer frame sirve
    /// para medir las filas y los siguientes aplican el centrado.
    fn dibujar_frames(
        radio: &mut Radio,
        ancho: f32,
        alto: f32,
        frames: usize,
    ) -> (Vec<(String, egui::Rect)>, egui::Rect) {
        let ctx = egui::Context::default();
        let pantalla = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(ancho, alto));
        let mut salida = None;
        for _ in 0..frames {
            salida = Some(ctx.run(
                egui::RawInput {
                    screen_rect: Some(pantalla),
                    ..Default::default()
                },
                |ctx| radio.dibujar(ctx),
            ));
        }
        (textos_pintados(&salida.unwrap().shapes), pantalla)
    }

    #[test]
    fn los_controles_se_pintan_dentro_y_centrados() {
        let mut radio = radio_de_prueba(20);
        // Se dibuja con la ventana en su tamaño mínimo.
        let (textos, pantalla) =
            dibujar_frames(&mut radio, VENTANA_MIN_ANCHO, VENTANA_MIN_ALTO, 3);

        // Las tres filas deben haberse medido (existen y se pintan).
        assert!(radio.ancho_fila_nav > 0.0, "fila de navegación sin medir");
        assert!(
            radio.ancho_fila_control > 0.0,
            "fila de reproducción sin medir"
        );
        assert!(radio.ancho_fila_timer > 0.0, "fila del temporizador sin medir");

        // Controles que el usuario echaba en falta: pausa, silencio y volumen.
        assert!(
            textos.iter().any(|(t, _)| t == "⏸"),
            "no se pintó el botón de pausa"
        );
        assert!(
            textos.iter().any(|(t, _)| t == "🔊" || t == "🔇"),
            "no se pintó el botón de silencio"
        );

        // La fila del temporizador debe quedar dentro de la ventana y centrada en
        // el panel central (que excluye el panel de emisoras de 240 px).
        let (_, rect_timer) = textos
            .iter()
            .find(|(t, _)| t.contains("Temporizador"))
            .expect("no se pintó el temporizador");
        assert!(
            rect_timer.max.y <= pantalla.max.y,
            "el temporizador se sale de la ventana: {rect_timer:?}"
        );
        let centro_panel = (pantalla.width() - PANEL_LISTA_ANCHO) * 0.5;
        assert!(
            (rect_timer.center().x - centro_panel).abs() < 6.0,
            "el temporizador no está centrado: {} vs {centro_panel}",
            rect_timer.center().x
        );

        // Todos los controles deben estar pintados y sin salirse por los lados
        // del panel central, ni siquiera con la ventana en su tamaño mínimo.
        let ancho_panel = pantalla.width() - PANEL_LISTA_ANCHO;
        let controles = [
            "⏮",
            "Noticias",
            "⏭",
            "⏸",
            "🔊",
            "🛑 Salir",
            "Temporizador",
        ];
        for control in controles {
            let (texto, rect) = textos
                .iter()
                .find(|(t, _)| t.contains(control))
                .unwrap_or_else(|| panic!("no se pintó el control {control:?}"));
            assert!(
                rect.min.x >= -0.5 && rect.max.x <= ancho_panel + 0.5,
                "el control {texto:?} se sale del panel central: {rect:?}"
            );
        }
    }

    /// El aviso/estado se pinta explícitamente y centrado (horizontal y
    /// verticalmente) entre los metadatos y los controles de volumen y salida.
    #[test]
    fn el_aviso_se_ve_centrado_entre_metadatos_y_controles() {
        let mut radio = radio_de_prueba(20);
        // Sin reproductor el estado es un aviso de error, que debe verse claro.
        let (textos, pantalla) = dibujar_frames(&mut radio, VENTANA_ANCHO, VENTANA_ALTO, 3);

        let buscar = |clave: &str| {
            textos
                .iter()
                .find(|(t, _)| t.contains(clave))
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("no se pintó {clave:?}"))
        };

        let meta = buscar("metadatos");
        let aviso = buscar("Audio no disponible");
        let nav = buscar("Noticias"); // primera fila de controles

        // Verticalmente queda en el hueco entre metadatos y controles.
        assert!(
            aviso.center().y > meta.bottom(),
            "el aviso ({}) no está por debajo de los metadatos ({})",
            aviso.center().y,
            meta.bottom()
        );
        assert!(
            aviso.center().y < nav.top(),
            "el aviso ({}) no está por encima de los controles ({})",
            aviso.center().y,
            nav.top()
        );

        // Horizontalmente queda centrado en el panel central.
        let centro_panel = (pantalla.width() - PANEL_LISTA_ANCHO) * 0.5;
        assert!(
            (aviso.center().x - centro_panel).abs() < 2.0,
            "el aviso no está centrado: {} vs {centro_panel}",
            aviso.center().x
        );
    }

    #[test]
    fn en_el_tamano_inicial_se_ven_al_menos_16_emisoras() {
        let mut radio = radio_de_prueba(40);
        let (textos, _) = dibujar_frames(&mut radio, VENTANA_ANCHO, VENTANA_ALTO, 2);
        let visibles = textos
            .iter()
            .filter(|(t, _)| t.contains("Emisora "))
            .count();
        assert!(
            visibles >= 16,
            "solo se ven {visibles} emisoras en el tamaño inicial"
        );
    }

    #[test]
    fn franja_noticias_normal() {
        assert!(en_franja_noticias(0, 0, 6));
        assert!(en_franja_noticias(5, 0, 6));
        assert!(!en_franja_noticias(6, 0, 6));
        assert!(!en_franja_noticias(12, 0, 6));
    }

    #[test]
    fn franja_noticias_cruza_medianoche() {
        assert!(en_franja_noticias(23, 22, 6));
        assert!(en_franja_noticias(2, 22, 6));
        assert!(!en_franja_noticias(12, 22, 6));
    }

    #[test]
    fn franja_noticias_desactivada() {
        assert!(!en_franja_noticias(10, 5, 5));
    }

    #[test]
    fn hueco_centrado_reparte_el_espacio_sobrante() {
        assert_eq!(hueco_centrado(400.0, 200.0), 100.0);
        assert_eq!(hueco_centrado(400.0, 0.0), 200.0);
        // Si el contenido no cabe, no se desplaza (evita recortes por la izquierda).
        assert_eq!(hueco_centrado(200.0, 400.0), 0.0);
    }

    #[test]
    fn controles_caben_en_la_ventana_minima() {
        // Con la altura mínima del panel central, los controles deben empezar lo
        // bastante arriba como para que las tres filas queden dentro.
        let altura_minima = 320.0_f32;
        let y = inicio_controles(0.0, altura_minima);
        assert!(y + ALTO_CONTROLES <= altura_minima + 0.001, "y={y}");

        // Con altura de sobra, se anclan al fondo.
        let y = inicio_controles(120.0, 500.0);
        assert_eq!(y, 500.0 - ALTO_CONTROLES);
    }

    #[test]
    fn parsea_emisoras_m3u() {
        let contenido =
            "#EXTM3U\n#EXTINF:-1,Emisora A\nhttp://a\n#EXTINF:-1,Emisora B\nhttp://b\n";
        let lista = parse_emisoras(contenido);
        assert_eq!(lista.len(), 2);
        assert_eq!(lista[0].nombre, "Emisora A");
        assert_eq!(lista[0].url, "http://a");
        assert_eq!(lista[1].nombre, "Emisora B");
        assert_eq!(lista[1].url, "http://b");
    }

    #[test]
    fn parsea_emisoras_sin_metadatos() {
        let lista = parse_emisoras("http://a\n\n#comentario\nhttp://b\n");
        assert_eq!(lista.len(), 2);
        assert_eq!(lista[0].nombre, "Desconocida");
        assert_eq!(lista[1].url, "http://b");
    }
}
