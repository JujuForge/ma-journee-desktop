#!/bin/sh
# Rafraichit le cache d'icones apres installation/mise a jour du .deb, pour que
# la barre des taches affiche immediatement la nouvelle icone (le tray Tauri la
# charge lui-meme au lancement et n'a pas ce probleme, mais l'icone resolue par
# l'environnement de bureau via le .desktop reste sinon en cache jusqu'a une
# deconnexion/reconnexion).
set -e

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
fi

exit 0
