//! Persistance JSON sur disque, independante de Tauri (donc testable sans `AppHandle`).
//!
//! Ecriture atomique (fichier temporaire + renommage) et lecture qui ne perd jamais
//! silencieusement un fichier illisible : il est journalise puis conserve a cote en `.corrupt`.

use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};

pub enum LoadResult<T> {
    Missing,
    Loaded(T),
    /// Inutilisable pour cette session : contenu invalide (mis de cote en `.corrupt`) ou
    /// lecture impossible (fichier laisse en place, voir `load_json`).
    Corrupt,
}

/// `<nom_du_fichier><suffixe>` dans le meme dossier (ex: `a.json` -> `a.json.tmp`).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Ecrit `contents` dans un fichier temporaire du meme dossier, le synchronise sur disque puis
/// le renomme vers `path`. Un plantage en cours d'ecriture laisse donc l'ancien fichier intact au
/// lieu d'un JSON tronque (contrairement a `std::fs::write`, qui tronque avant d'ecrire).
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = sibling(path, ".tmp");
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    let json = serde_json::to_vec(value).map_err(|e| format!("serialisation: {e}"))?;
    write_atomic(path, &json).map_err(|e| format!("ecriture de {}: {e}", path.display()))
}

/// Conserve un fichier illisible en `<nom>.corrupt` (remplace une copie precedente) pour qu'il
/// puisse etre recupere a la main, et libere l'emplacement d'origine.
fn quarantine(path: &Path) {
    let target = sibling(path, ".corrupt");
    match fs::rename(path, &target) {
        Ok(()) => log::error!("Fichier illisible conserve dans {}", target.display()),
        Err(e) => log::error!("Impossible de conserver {} en .corrupt: {}", path.display(), e),
    }
}

/// Lit et decode un fichier JSON. Un fichier absent n'est pas une erreur ; un contenu invalide
/// est journalise (position seulement, jamais le contenu : il peut contenir des donnees
/// utilisateur) et mis de cote. Une lecture impossible (droits, verrou d'antivirus...) laisse le
/// fichier en place : son contenu est peut-etre valide et sera relu au prochain lancement.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> LoadResult<T> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LoadResult::Missing,
        // Contenu non UTF-8 : le fichier lui-meme est en cause.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            log::error!("Contenu illisible dans {}: {}", path.display(), e);
            quarantine(path);
            return LoadResult::Corrupt;
        }
        Err(e) => {
            log::error!("Lecture impossible de {} (fichier conserve tel quel): {}", path.display(), e);
            return LoadResult::Corrupt;
        }
    };
    match serde_json::from_str(&content) {
        Ok(value) => LoadResult::Loaded(value),
        Err(e) => {
            log::error!(
                "JSON invalide dans {} ({:?}, ligne {}, colonne {})",
                path.display(),
                e.classify(),
                e.line(),
                e.column()
            );
            quarantine(path);
            LoadResult::Corrupt
        }
    }
}

/// Dossier temporaire unique, supprime a la fin du test (meme en cas d'echec).
#[cfg(test)]
pub struct TempDir(pub PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "majournee-test-{}-{}-{}",
            std::process::id(),
            tag,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Sample {
        name: String,
        count: u32,
    }

    fn sample() -> Sample {
        Sample { name: "a".to_string(), count: 3 }
    }

    // ── write_atomic() ───────────────────────────────────────────────────────

    #[test]
    fn write_atomic_creates_file_and_missing_parent_dirs() {
        let tmp = TempDir::new("create");
        let path = tmp.0.join("sub").join("data.json");
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
    }

    #[test]
    fn write_atomic_replaces_existing_file_without_leaving_tmp() {
        let tmp = TempDir::new("replace");
        let path = tmp.0.join("data.json");
        write_atomic(&path, b"old content, longer than the new one").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(!sibling(&path, ".tmp").exists());
    }

    // ── save_json() / load_json() ────────────────────────────────────────────

    #[test]
    fn json_round_trip() {
        let tmp = TempDir::new("roundtrip");
        let path = tmp.0.join("data.json");
        save_json(&path, &sample()).unwrap();
        match load_json::<Sample>(&path) {
            LoadResult::Loaded(s) => assert_eq!(s, sample()),
            _ => panic!("fichier valide non charge"),
        }
    }

    #[test]
    fn load_missing_file_is_not_corrupt() {
        let tmp = TempDir::new("missing");
        assert!(matches!(load_json::<Sample>(&tmp.0.join("absent.json")), LoadResult::Missing));
    }

    #[test]
    fn load_truncated_json_is_quarantined() {
        let tmp = TempDir::new("truncated");
        let path = tmp.0.join("data.json");
        fs::write(&path, r#"{"name":"a","cou"#).unwrap();
        assert!(matches!(load_json::<Sample>(&path), LoadResult::Corrupt));
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(sibling(&path, ".corrupt")).unwrap(), r#"{"name":"a","cou"#);
    }

    #[test]
    fn load_wrong_shape_is_quarantined() {
        let tmp = TempDir::new("shape");
        let path = tmp.0.join("data.json");
        fs::write(&path, r#"{"name":"a","count":"pas un nombre"}"#).unwrap();
        assert!(matches!(load_json::<Sample>(&path), LoadResult::Corrupt));
        assert!(sibling(&path, ".corrupt").exists());
    }

    #[test]
    fn load_non_utf8_file_is_quarantined() {
        let tmp = TempDir::new("non-utf8");
        let path = tmp.0.join("data.json");
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(load_json::<Sample>(&path), LoadResult::Corrupt));
        assert!(!path.exists());
        assert!(sibling(&path, ".corrupt").exists());
    }

    #[test]
    fn unreadable_path_is_left_in_place() {
        // Un dossier a la place du fichier : la lecture echoue sans que le contenu soit en cause.
        let tmp = TempDir::new("unreadable");
        let path = tmp.0.join("data.json");
        fs::create_dir(&path).unwrap();
        assert!(matches!(load_json::<Sample>(&path), LoadResult::Corrupt));
        assert!(path.is_dir());
        assert!(!sibling(&path, ".corrupt").exists());
    }

    #[test]
    fn a_new_corrupt_file_replaces_the_previous_copy() {
        let tmp = TempDir::new("replace-corrupt");
        let path = tmp.0.join("data.json");
        fs::write(&path, "premier").unwrap();
        assert!(matches!(load_json::<Sample>(&path), LoadResult::Corrupt));
        fs::write(&path, "second").unwrap();
        assert!(matches!(load_json::<Sample>(&path), LoadResult::Corrupt));
        assert_eq!(fs::read_to_string(sibling(&path, ".corrupt")).unwrap(), "second");
    }
}
