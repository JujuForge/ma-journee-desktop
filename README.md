# Ma Journée Desktop

Application de bureau Ma Journée pour Windows et Linux. C'est une enveloppe native (Tauri) qui charge [majournee.com](https://majournee.com).

Ce dépôt contient uniquement l'enveloppe desktop :
- la fenêtre native
- les notifications système
- l'icône dans la zone de notification
- le démarrage automatique
- les mises à jour

La logique de Ma Journée (tâches, notes, révisions espacées, emploi du temps…) est hébergée sur majournee.com et n'est pas incluse ici.

## Prérequis

- [Rust](https://rustup.rs/) (version stable)
- [Node.js](https://nodejs.org/) (pour la CLI Tauri)

**Linux** (Debian/Ubuntu) :

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev
```

**Windows** :
- [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/), avec la charge de travail « Développement Desktop en C++ »
- WebView2, déjà présent sur Windows 10 et 11

## Compiler

```bash
npm install
npx tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

Les installateurs sont générés dans `src-tauri/target/release/bundle/` :
- `deb/`, `rpm/` et `appimage/` sous Linux
- `nsis/` et `msi/` sous Windows

Pour ne produire qu'un seul format, ajoutez `--bundles deb` (ou `nsis`, etc.).

> **Pourquoi l'option `--config` ?** Un simple `npx tauri build` produit bien les installateurs, mais se termine par l'erreur
> `A public key has been found, but no private key`. Elle vient de la signature des mises à jour automatiques, qui exige la clé
> privée du mainteneur (variable `TAURI_SIGNING_PRIVATE_KEY`). L'option ci-dessus désactive cette étape, inutile pour un build local.
>
> Elle ne désactive pas les mises à jour de l'app installée : celle-ci continue de recevoir les versions officielles. Elle empêche
> seulement ce build de produire les fichiers signés nécessaires pour *publier* une mise à jour.

## Développer en local

```bash
npm run dev
```

Cette commande construit puis sert localement la web app, et non majournee.com :
- elle attend le dépôt de la web app à côté, dans `../../V067plusWeb`
- elle a besoin de `python3`

L'app de développement :
- charge `http://localhost:1420`
- s'affiche avec le titre « Ma Journée (DEV) »
- utilise un identifiant séparé (`fr.jujuforge.majournee.desktop.dev`), donc elle ne touche pas aux réglages de la version installée

## Vérifier avant de proposer une modification

Depuis `src-tauri/`, ce sont les mêmes contrôles que la CI :

```bash
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo audit   # nécessite : cargo install cargo-audit
```

## Publication

Les versions officielles sont publiées par le mainteneur, avec la clé de signature des mises à jour. Elles ne se font pas depuis ce dépôt.

## Licence

MIT, voir [LICENSE](LICENSE).
