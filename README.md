# Ma Journée Desktop

Application de bureau Ma Journée disponible sur Windows et Linux. Enveloppe native (Tauri) qui charge majournee.com.

Ce dépôt contient uniquement l'enveloppe desktop : fenêtre native, notifications système, icône de zone de notification, démarrage automatique, mises à jour. La logique applicative de Ma Journée (tâches, notes, révisions espacées, emploi du temps, etc.) est hébergée sur [majournee.com](https://majournee.com) et n'est pas incluse ici.

## Compilation

Prérequis : [Rust](https://rustup.rs/), Node.js.

```bash
npm install
npx tauri build
```

## Licence

MIT, voir [LICENSE](LICENSE).
