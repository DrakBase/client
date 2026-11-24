# Construction et essai de la démo Tauri sous Linux (AppImage), dans Docker (OrbStack sur le Mac).
# Voir ../README.md, « Essai sous Linux ».
#
# Node vient de l'image officielle : celui de Debian 12 (18.x) est trop ancien pour Vite 8. C'est
# Node 24, le minimum que demandent le `package.json` de la démo et celui de l'aide (`engines`).
FROM node:24-bookworm AS node

FROM rust:1.99.1-bookworm

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# Dépendances de Tauri 2 sous Linux, plus de quoi lancer l'app sans écran : Xvfb pour
# l'affichage, et une session D-Bus pour le plugin d'instance unique.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev \
        file patchelf xdg-utils \
        xvfb xauth dbus dbus-x11 procps \
    && rm -rf /var/lib/apt/lists/*

# Pas de FUSE dans un conteneur : les AppImage (celle de l'app, et linuxdeploy pendant la
# construction) s'extraient puis se lancent.
ENV APPIMAGE_EXTRACT_AND_RUN=1 \
    NO_STRIP=true

RUN useradd --create-home --uid 1000 demo
USER demo
WORKDIR /home/demo
