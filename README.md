<p align="center"><img src="docs/logo.png" width="128" alt="Kraken Mount"></p>

Kraken Mount

Fork de quer3q/kraken-mount, mantenido por modgames44. Este fork sigue el código base de upstream (0.4.0: instalador nativo, reporte de crashes, auto-actualización, ocultar juegos y backports) y además añade:

Telemetría en vivo (CPU del sistema y del payload + RAM), expuesta en /api/telemetry.
Un tema visual propio, "The Abyss": paleta abisal (teal bioluminiscente + azul mar) con fondo de océano.
Empaqueta ampr-emu-ng 0.5.7 (la 0.5.1 de quer3q + decodificación LZ4 rápida opcional).

Un montador de juegos para PS5. Encuentra juegos en el SSD interno, el disco M.2 y unidades USB.

Formatos soportados: FPKG debug (.pkg), carpetas de juego (planas, o empaquetadas con zstd o LZ4) e imágenes exFAT (.exfat, o .zstd que contenga una imagen exFAT), montadas en solo lectura como lo hace ShadowMount+. Para juegos grandes y exigentes, usa el formato de empaquetado zstd: rinde mejor que FPKG.

Cómo usar
Descarga kraken-mount.elf de Releases y envíalo a tu PS5 jailbrekeada con el cargador de ELF (elfldr, puerto 9021) o payload-manager: nc -w 3 <ip-ps5> 9021 < kraken-mount.elf. kstuff debe estar cargado y ShadowMount+ no debe estar corriendo: Kraken Mount espera hasta que ambas condiciones se cumplan.
Pon cada juego en una raíz de escaneo, o en una carpeta directamente dentro de una (por ejemplo /data/homebrew/rpg/<juego>; no se escanean carpetas más profundas, y en la raíz de una unidad solo la raíz misma):
SSD interno: /data/homebrew, /data/etaHEN/games
M.2 o disco extendido (/mnt/ext0, /mnt/ext1): homebrew, etaHEN/games o la raíz de la unidad
USB (/mnt/usb0 a /mnt/usb7): homebrew, etaHEN/games o la raíz de la unidad
Cada juego recibe una notificación de "registered" y su propia tarjeta en el menú principal.

ampr-emu-ng viene incluido y se instala en /data/krakenmount/ampr/ al arrancar. Para usar otra build, pon su libSceAmpr.sprx ahí; se conserva hasta que lo borres.

Un juego que necesita el emulador de DLC o PlayGo y no trae ninguno recibe el incluido en su fakelib (ajuste add_emulators, activo por defecto); la copia propia del juego se conserva, salvo que sea una versión conocida más antigua de la incluida, que se actualiza. El stub de PlayGo se puede desactivar por juego en los detalles del juego. Detalles: emulador DLC, stub PlayGo.

Para usar otra build en un solo juego (una build debug, por ejemplo), ponla en /data/krakenmount/ampr-titles/<TITLE_ID>/; una carpeta vacía ahí corre ese juego sin ampr-emu-ng. Tras cada lanzamiento, Kraken Mount lee /data/ampr_emu_status.txt y avisa cuando ampr-emu-ng no cargó o reportó un problema (detalles en el log).

Depurar un juego: añade su title id a "Debug ampr-emu-ng for these games" en los ajustes (ampr_debug_titles). Ese juego corre entonces la build debug incluida, y mientras corre Kraken Mount guarda sus métricas en /data/krakenmount/debug/<TITLE_ID>/ (también en http://<ip-ps5>:8097/api/debug/<TITLE_ID>/metrics.txt).

Las caches viven en /data/krakenmount/caches/<TITLE_ID>, los ajustes en /data/krakenmount/config.toml y el log en /data/krakenmount/kraken.log.

Para compilarlo tú mismo: ps5/build.sh (necesita Docker), salida en target/ps5/kraken-mount.elf. En WSL/Linux sin Docker, este fork incluye compilar_kraken_wsl.sh (reproduce las dos etapas del build oficial).

Créditos
drakmor/ShadowMountPlus: el registro en el menú principal, el overlay sandbox de la fakelib y los backports están portados de ahí.
drakmor/dlc_emu: el emulador de DLC incluido (libSceAppContent.sprx, libSceNpEntitlementAccess.sprx, libSceGameUpdate.sprx), añadido a la fakelib de un juego cuando este tiene un dlc_emu.ini en su raíz; una versión conocida más antigua que traiga el juego se actualiza, cualquier otra copia se conserva.
drakmor/pgo_stub: el stub de PlayGo incluido (libScePlayGo.sprx), añadido a la fakelib cuando el eboot.bin importa libScePlayGo y el juego no trae ninguno.
devchmod/TITAN-CORE (GPL-3.0-or-later): el muestreador de telemetría en vivo (telemetry.rs, telemetry_math.rs) está portado de ahí.
ps5-payload-sdk: el toolchain del PS5.
Licencia

GPL-3.0-or-later (LICENSE). Los avisos de terceros están en THIRD_PARTY_NOTICES.
