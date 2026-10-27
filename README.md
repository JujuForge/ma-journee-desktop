# Ma Journée Desktop

Application de bureau [Ma Journée](https://majournee.com) pour Windows et Linux : une enveloppe Tauri qui charge majournee.com et ajoute la fenêtre native, les notifications, l'icône de la zone de notification, le démarrage automatique et les mises à jour.

La logique de l'application est hébergée sur majournee.com et n'est pas dans ce dépôt.

## Prérequis

- [Rust](https://rustup.rs/) et [Node.js](https://nodejs.org/)
- Linux (Debian/Ubuntu) :
  ```bash
  sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev
  ```
- Windows : [C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) (« Développement Desktop en C++ »)

## Compiler

```bash
npm install
npx tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

Les installateurs sont générés dans `src-tauri/target/release/bundle/`. Pour un seul format : `--bundles deb` (ou `nsis`…).

L'option `--config` évite l'erreur finale `no private key` : sans la clé privée du mainteneur, Tauri ne peut pas signer les fichiers de publication des mises à jour. L'app compilée reçoit quand même les mises à jour officielles.

## Vérifier

Avant de proposer une modification, depuis `src-tauri/` (mêmes contrôles que la CI) :

```bash
cargo fmt --check && cargo test --locked && cargo clippy --all-targets --locked -- -D warnings
cargo audit   # cargo install cargo-audit
```

## Verrouillage

Menu **Fichier > Verrouillage…** : un code (et Windows Hello sous Windows) est demandé au démarrage, après la fermeture de la fenêtre et au réveil de l'ordinateur. Le contenu des notifications est masqué tant que l'app est verrouillée. En cas d'oubli, « Code oublié ? » supprime le verrou et déconnecte le compte sur cet ordinateur.

C'est un écran de confidentialité, pas un chiffrement : il protège la session ouverte, pas les fichiers du compte système. Sur un ordinateur partagé, des comptes système distincts restent la vraie séparation.

## Licence

MIT, voir [LICENSE](LICENSE). Les versions officielles sont publiées par le mainteneur.
