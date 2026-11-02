# Capturas gráficas

Este directorio guarda las capturas de pantalla de la aplicación. Se referencian
desde el [`README.md`](../README.md) principal, en el apartado «El programa
funcionando».

## Archivos

| Archivo | Qué muestra |
| --- | --- |
| `pantalla-lista.png` | Ventana principal con la lista lateral de emisoras. |
| `pantalla-aviso.png` | Aviso/estado centrado entre los metadatos y los controles. |
| `pantalla-reproduciendo.png` | Estado «🎵 Reproduciendo». |
| `pantalla-conectando.png` | Estado «⏳ Conectando…». |
| `pantalla-metadatos.png` | Metadatos ICY en la fila desplazable. |
| `pantalla-minima.png` | Ventana en tamaño mínimo, con los controles centrados. |

## Cómo regenerarlas

Si cambia la interfaz, vuelve a tomar las capturas y sustituye los PNG
conservando los **mismos nombres** (si no, enlaces del `README.md` se rompen).

- **Wayland:** `grim` o `gnome-screenshot`
- **X11:** `scrot`, `import` (ImageMagick) o la tecla `ImpPnt` del teclado

Conviene capturar:

1. La ventana en el tamaño inicial (`700×500`).
2. El estado de error centrado: selecciona una emisora con la URL caída para que
   aparezca el aviso.
3. La ventana reducida al tamaño mínimo (`520×320`).
