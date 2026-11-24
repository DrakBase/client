//! Systèmes, architectures, types d'artefacts et canaux, sous les noms du plugin Tauri.

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("valeur « {value} » inconnue pour {kind}")]
pub struct UnknownValue {
    pub kind: &'static str,
    pub value: String,
}

/// Noms textuels d'une énumération : `as_str`, `Display`, `FromStr` (erreur [`UnknownValue`]) et
/// serde au même endroit. Deux formes :
///
/// - `named_enum!(Ty, "le genre", { Variante => "nom", … })` pour une énumération déjà déclarée ;
/// - `named_enum!(/// doc\n pub enum Ty, "le genre", { … })`, qui la déclare aussi, avec
///   `Clone`, `Copy`, `Debug`, `PartialEq`, `Eq` et `Hash`.
#[macro_export]
macro_rules! named_enum {
    ($(#[$meta:meta])* $vis:vis enum $ty:ident, $kind:literal, { $($(#[$variant_meta:meta])* $variant:ident => $name:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        $vis enum $ty {
            $($(#[$variant_meta])* $variant),+
        }

        $crate::named_enum!($ty, $kind, { $($variant => $name),+ });
    };
    ($ty:ident, $kind:literal, { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl $ty {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $($ty::$variant => $name,)+
                }
            }
        }

        impl ::std::fmt::Display for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $ty {
            type Err = $crate::UnknownValue;

            fn from_str(value: &str) -> ::std::result::Result<Self, Self::Err> {
                match value {
                    $($name => Ok($ty::$variant),)+
                    _ => Err($crate::UnknownValue { kind: $kind, value: value.to_owned() }),
                }
            }
        }

        impl $crate::__serde::Serialize for $ty {
            fn serialize<S: $crate::__serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> $crate::__serde::Deserialize<'de> for $ty {
            fn deserialize<D: $crate::__serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                let raw = <::std::string::String as $crate::__serde::Deserialize>::deserialize(
                    deserializer,
                )?;
                raw.parse().map_err($crate::__serde::de::Error::custom)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Os {
    Darwin,
    Windows,
    Linux,
}

named_enum!(Os, "l'OS", { Darwin => "darwin", Windows => "windows", Linux => "linux" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Arch {
    X86_64,
    Aarch64,
    I686,
    Armv7,
    Riscv64,
    /// Binaire universel macOS : sert aarch64 et x86_64. Jamais envoyé par un client.
    Universal,
}

named_enum!(Arch, "l'architecture", {
    X86_64 => "x86_64",
    Aarch64 => "aarch64",
    I686 => "i686",
    Armv7 => "armv7",
    Riscv64 => "riscv64",
    Universal => "universal",
});

impl Arch {
    /// Arch envoyée par un client : toutes sauf `universal`.
    pub fn parse_client(value: &str) -> Result<Self, UnknownValue> {
        match value.parse::<Arch>()? {
            Arch::Universal => Err(UnknownValue {
                kind: "l'architecture",
                value: value.to_owned(),
            }),
            arch => Ok(arch),
        }
    }

    /// Cette arch a-t-elle un sens pour un artefact de cet OS ?
    pub const fn supported_on(self, os: Os) -> bool {
        matches!(
            (os, self),
            (Os::Darwin, Arch::X86_64 | Arch::Aarch64 | Arch::Universal)
                | (Os::Windows, Arch::X86_64 | Arch::Aarch64 | Arch::I686)
                | (
                    Os::Linux,
                    Arch::X86_64 | Arch::Aarch64 | Arch::I686 | Arch::Armv7 | Arch::Riscv64
                )
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BundleType {
    /// `.app.tar.gz` : mise à jour macOS.
    App,
    /// `.dmg` : première installation macOS seulement.
    Dmg,
    Nsis,
    Msi,
    AppImage,
    Deb,
    Rpm,
    /// `.stack.tar.gz` : archive d'une stack Docker (app de type `stack`), sous Linux.
    Stack,
    /// Exécutable d'une app de type `cli`, sous Linux.
    Bin,
}

named_enum!(BundleType, "le type de bundle", {
    App => "app",
    Dmg => "dmg",
    Nsis => "nsis",
    Msi => "msi",
    AppImage => "appimage",
    Deb => "deb",
    Rpm => "rpm",
    Stack => "stack",
    Bin => "bin",
});

impl BundleType {
    pub const fn os(self) -> Os {
        match self {
            BundleType::App | BundleType::Dmg => Os::Darwin,
            BundleType::Nsis | BundleType::Msi => Os::Windows,
            BundleType::AppImage
            | BundleType::Deb
            | BundleType::Rpm
            | BundleType::Stack
            | BundleType::Bin => Os::Linux,
        }
    }

    /// Le DMG ne sert qu'à la première installation ; tous les autres types servent aussi de mise à jour.
    pub const fn is_update_payload(self) -> bool {
        !matches!(self, BundleType::Dmg)
    }

    /// Rang pour la première installation sur la page publique : 0 est le meilleur pour son OS.
    pub const fn installer_rank(self) -> u8 {
        match self {
            BundleType::Dmg
            | BundleType::Nsis
            | BundleType::AppImage
            | BundleType::Stack
            | BundleType::Bin => 0,
            BundleType::App | BundleType::Msi | BundleType::Deb => 1,
            BundleType::Rpm => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

named_enum!(Channel, "le canal", { Stable => "stable", Beta => "beta" });

impl Channel {
    /// Canal demandé par un client : toute valeur inconnue vaut `stable`.
    pub fn parse_client(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(raw) if raw.eq_ignore_ascii_case("beta") => Channel::Beta,
            _ => Channel::Stable,
        }
    }

    /// Une version publiée sur `release` est-elle visible depuis ce canal ? La bêta voit tout.
    pub const fn includes(self, release: Channel) -> bool {
        matches!(
            (self, release),
            (_, Channel::Stable) | (Channel::Beta, Channel::Beta)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for bundle in [
            BundleType::App,
            BundleType::Dmg,
            BundleType::Nsis,
            BundleType::Msi,
            BundleType::AppImage,
            BundleType::Deb,
            BundleType::Rpm,
            BundleType::Stack,
            BundleType::Bin,
        ] {
            assert_eq!(bundle.as_str().parse::<BundleType>().unwrap(), bundle);
        }
        assert_eq!(serde_json::to_string(&Arch::X86_64).unwrap(), "\"x86_64\"");
    }

    #[test]
    fn client_values() {
        assert!(Arch::parse_client("universal").is_err());
        assert_eq!(Channel::parse_client(Some("Beta")), Channel::Beta);
        assert_eq!(Channel::parse_client(Some("nightly")), Channel::Stable);
    }

    #[test]
    fn beta_sees_both_channels() {
        assert!(Channel::Beta.includes(Channel::Stable));
        assert!(Channel::Beta.includes(Channel::Beta));
        assert!(!Channel::Stable.includes(Channel::Beta));
    }

    #[test]
    fn universal_only_on_macos() {
        assert!(Arch::Universal.supported_on(Os::Darwin));
        assert!(!Arch::Universal.supported_on(Os::Linux));
        assert!(!Arch::Armv7.supported_on(Os::Windows));
    }
}
