//! Contrat partagé entre le serveur, l'outil `drakbase`, l'agent `drakbase-stack` et les tests :
//! versions, plateformes, réponse de mise à jour, signatures minisign au format Tauri, stacks
//! Docker, objets de l'API de publication, et les règles et limites que plusieurs crates
//! appliquent (noms, versions d'OS, en-tête d'un ELF, fiche publique, adresse d'un serveur,
//! écriture d'une taille, texte rendu visible avant d'être affiché).
//! Aucune entrée/sortie ici.
//! La spécification fait foi : `docs/PROTOCOLE.md`, et `docs/API.md` pour la publication.
//!
//! La feature `signing` ajoute la création de clés et la signature (`signing`, crate
//! `minisign`) : l'outil l'active, le serveur et l'agent seulement pour leurs essais.

pub mod elf;
pub mod listing;
pub mod names;
pub mod os_version;
pub mod platform;
pub mod publish;
pub mod server;
pub mod signature;
#[cfg(feature = "signing")]
pub mod signing;
pub mod size;
pub mod stack;
pub mod update;
pub mod version;
pub mod visible;

pub use elf::{elf_arch, elf_is_executable, is_elf};
pub use names::{is_app_slug, sanitize_filename};
pub use os_version::OsVersion;
pub use platform::{Arch, BundleType, Channel, Os, UnknownValue};
pub use server::{PRODUCTION_SERVER, ServerUrl};
pub use size::human_size;
pub use version::{Version, VersionError};
pub use visible::{has_hidden, is_hidden, visible, visible_lines};

/// Pour `named_enum!`, qui écrit des implémentations de serde dans d'autres crates.
#[doc(hidden)]
pub use serde as __serde;
