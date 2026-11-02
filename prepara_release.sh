#!/usr/bin/env bash
#
# prepara_release.sh — Prepara una versión local del reproductor de radio.
#
# Qué hace, en orden:
#   1. Compila en modo release (lto + strip, ya definidos en Cargo.toml).
#   2. Copia el ejecutable a distribucion/bin/.
#   3. Regenera distribucion/bin/README.pdf si hay un convierte_a_pdf.sh a mano.
#   4. Empaqueta distribucion/ en dist/radio-gui-rust-v<versión>.zip (+ .sha256).
#   5. Con --etiqueta, crea la etiqueta git anotada v<versión>.
#
# Uso:
#   ./prepara_release.sh                  # compila y empaqueta
#   ./prepara_release.sh --sin-compilar   # empaqueta con el binario ya existente
#   ./prepara_release.sh --etiqueta       # además crea la etiqueta git anotada
#   ./prepara_release.sh --ayuda          # muestra esta ayuda
#
# El zip resultante NO se versiona (tampoco el ejecutable ni el PDF): el
# repositorio solo guarda código y ficheros pequeños de configuración, para
# que no crezca con cada versión. El zip es lo que se adjunta al Release.
#
set -euo pipefail

cd "$(dirname "$0")"

USO="$(cat <<'EOF'
prepara_release.sh — Prepara una versión local del reproductor de radio.

Uso:
  ./prepara_release.sh                  compila y empaqueta
  ./prepara_release.sh --sin-compilar   empaqueta con el binario ya existente
  ./prepara_release.sh --etiqueta       además crea la etiqueta git anotada
  ./prepara_release.sh --ayuda          muestra esta ayuda

Salida:
  dist/radio-gui-rust-v<versión>.zip        paquete listo para el Release
  dist/radio-gui-rust-v<versión>.zip.sha256 suma de verificación
EOF
)"

COMPILAR=1
ETIQUETAR=0
for arg in "$@"; do
    case "$arg" in
        --sin-compilar) COMPILAR=0 ;;
        --etiqueta) ETIQUETAR=1 ;;
        --ayuda | -h) printf '%s\n' "$USO"; exit 0 ;;
        *) echo "Opción desconocida: $arg (prueba --ayuda)" >&2; exit 2 ;;
    esac
done

# --- Versión declarada en Cargo.toml ---------------------------------------
VERSION=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
if [ -z "$VERSION" ]; then
    echo "No encuentro 'version = "..."' en Cargo.toml" >&2
    exit 1
fi
ETIQUETA="v$VERSION"
PAQUETE="radio-gui-rust-$ETIQUETA"

echo "==> Preparando $PAQUETE"

# --- 1. Compilar ------------------------------------------------------------
if [ "$COMPILAR" -eq 1 ]; then
    echo "==> cargo build --release"
    cargo build --release
    cp target/release/radio-gui-rust distribucion/bin/radio-gui-rust
else
    echo "==> Se omite la compilación (--sin-compilar)"
fi
if [ ! -x distribucion/bin/radio-gui-rust ]; then
    echo "No hay ejecutable en distribucion/bin/" >&2
    exit 1
fi

# --- 2. Documentación en PDF ------------------------------------------------
if [ -x $HOME/bin/convierte_md_a_pdf.sh ]; then
    echo "==> Generando distribucion/bin/README.pdf"
    convierte_md_a_pdf.sh  README.md  
elif [ -f distribucion/bin/README.pdf ]; then
    echo "==> Sin convierte_a_pdf.sh a mano: se conserva el PDF actual"
else
    echo "==> Aviso: no hay README.pdf; si lo quieres incluir, ejecuta tu guión y repite"
fi

# --- 3. Empaquetar ----------------------------------------------------------
mkdir -p dist
rm -f "dist/$PAQUETE.zip" "dist/$PAQUETE.zip.sha256"
echo "==> Creando dist/$PAQUETE.zip"
zip -qr "dist/$PAQUETE.zip" distribucion
(cd dist && sha256sum "$PAQUETE.zip" > "$PAQUETE.zip.sha256")

echo "==> Contenido del paquete:"
unzip -l "dist/$PAQUETE.zip" | sed -n '4,$p' | head -n -2

# --- 4. Estado del repositorio ---------------------------------------------
SUCIO=$(git status --porcelain -- distribucion src Cargo.toml README.md 2>/dev/null || true)
if [ -n "$SUCIO" ]; then
    echo "==> Aviso: hay cambios de esta versión sin 'git commit'."
else
    echo "==> Repositorio limpio en los ficheros de la versión."
fi

if [ "$ETIQUETAR" -eq 1 ]; then
    if git rev-parse -q --verify "refs/tags/$ETIQUETA" >/dev/null; then
        echo "==> La etiqueta $ETIQUETA ya existe; no se repite."
    else
        if [ -n "$(git status --porcelain)" ]; then
            echo "==> No se crea la etiqueta: primero haz 'git commit' de todo." >&2
        else
            echo "==> Creando etiqueta anotada $ETIQUETA"
            git tag -a "$ETIQUETA" -m "Reproductor de radio $VERSION"
        fi
    fi
fi

# --- 5. Siguientes pasos ----------------------------------------------------
RUTA="dist/$PAQUETE.zip"
TAM=$(du -h "$RUTA" | cut -f1)
cat <<EOF

Listo: $RUTA  ($TAM)
Suma:  $(cut -d' ' -f1 "dist/$PAQUETE.zip.sha256")

Siguientes pasos (ver RELEASING.md para el detalle):

  1. git add -A && git commit -m "descripcion de la versión"
  2. git tag -a $ETIQUETA -m "Reproductor de radio $VERSION"
  3. Sube el repositorio a GitHub:
       gh repo create radio-gui-rust --public --source=. --remote=origin --push
       git push origin $ETIQUETA
  4. Publica el Release con el paquete:
       gh release create $ETIQUETA "$RUTA" \\
         --title "Radio GUI $VERSION" --notes "Notas de la versión"
EOF
