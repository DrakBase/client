//! Exécutable Linux, d'après son en-tête ELF : son arch, et s'il s'exécute. Les mêmes règles pour
//! le serveur (envoi d'un artefact `bin`), l'outil (avant l'envoi) et l'agent (`self-update`).

use crate::Arch;

/// Octets d'en-tête qu'il faut pour lire le type et l'arch : jusqu'à `e_machine` compris.
pub const ELF_HEADER_LEN: usize = 20;

const MAGIC: &[u8] = b"\x7fELF";
const CLASS_32: u8 = 1;
const CLASS_64: u8 = 2;
const DATA_LITTLE_ENDIAN: u8 = 1;
const DATA_BIG_ENDIAN: u8 = 2;
/// `e_type` d'un exécutable à adresse fixe.
const TYPE_EXEC: u16 = 2;
/// `e_type` d'un exécutable indépendant de sa position (PIE), ou d'une bibliothèque partagée.
const TYPE_DYN: u16 = 3;

/// Le fichier commence-t-il comme un ELF ? Sert à dire « pas un exécutable Linux » plutôt que
/// « d'une autre architecture ».
pub fn is_elf(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

/// Mot de 16 bits de l'en-tête, dans le boutisme qu'il annonce ; `None` si l'en-tête est trop
/// court, n'est pas celui d'un ELF, ou annonce un boutisme inconnu.
fn word(head: &[u8], at: usize) -> Option<u16> {
    if head.len() < ELF_HEADER_LEN || !is_elf(head) {
        return None;
    }
    let bytes = [head[at], head[at + 1]];
    match head[5] {
        DATA_LITTLE_ENDIAN => Some(u16::from_le_bytes(bytes)),
        DATA_BIG_ENDIAN => Some(u16::from_be_bytes(bytes)),
        _ => None,
    }
}

/// L'ELF est-il un exécutable, statique ou non, PIE compris ? D'après `e_type` : un fichier
/// objet (`.o`) ou une image mémoire ne l'est pas.
pub fn elf_is_executable(head: &[u8]) -> bool {
    matches!(word(head, 16), Some(TYPE_EXEC | TYPE_DYN))
}

/// Arch d'un ELF, d'après les premiers octets du fichier (`ELF_HEADER_LEN` au moins) : la
/// machine (`e_machine`) et la classe (32 ou 64 bits) doivent aller ensemble. `None` si ce n'est
/// pas un ELF, ou pour une arch hors de la table : un x86-64 en 32 bits (x32) n'est pas `x86_64`.
pub fn elf_arch(head: &[u8]) -> Option<Arch> {
    match (word(head, 18)?, head[4]) {
        (62, CLASS_64) => Some(Arch::X86_64),
        (183, CLASS_64) => Some(Arch::Aarch64),
        (243, CLASS_64) => Some(Arch::Riscv64),
        (40, CLASS_32) => Some(Arch::Armv7),
        (3, CLASS_32) => Some(Arch::I686),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(class: u8, data: u8, machine: u16) -> Vec<u8> {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', class, data, 1, 0];
        bytes.resize(18, 0);
        bytes.extend(match data {
            DATA_BIG_ENDIAN => machine.to_be_bytes(),
            _ => machine.to_le_bytes(),
        });
        bytes
    }

    #[test]
    fn machine_and_class_give_the_arch() {
        let cases = [
            (CLASS_64, 62, Some(Arch::X86_64)),
            (CLASS_64, 183, Some(Arch::Aarch64)),
            (CLASS_64, 243, Some(Arch::Riscv64)),
            (CLASS_32, 40, Some(Arch::Armv7)),
            (CLASS_32, 3, Some(Arch::I686)),
            // La classe compte : x32, ILP32 et RISC-V 32 bits sont hors de la table.
            (CLASS_32, 62, None),
            (CLASS_32, 183, None),
            (CLASS_32, 243, None),
            (CLASS_64, 40, None),
            (CLASS_64, 8, None),
        ];
        for (class, machine, expected) in cases {
            for data in [DATA_LITTLE_ENDIAN, DATA_BIG_ENDIAN] {
                let bytes = header(class, data, machine);
                assert_eq!(elf_arch(&bytes), expected, "{class} {machine} {data}");
                assert!(is_elf(&bytes));
            }
        }
    }

    #[test]
    fn only_executables_and_pie_are_executable() {
        for data in [DATA_LITTLE_ENDIAN, DATA_BIG_ENDIAN] {
            // Dans l'ordre : aucun type, objet (`.o`), exécutable, PIE, image mémoire.
            for (kind, expected) in [(0u16, false), (1, false), (2, true), (3, true), (4, false)] {
                let mut bytes = header(CLASS_64, data, 183);
                let kind_bytes = match data {
                    DATA_BIG_ENDIAN => kind.to_be_bytes(),
                    _ => kind.to_le_bytes(),
                };
                bytes[16..18].copy_from_slice(&kind_bytes);
                assert_eq!(elf_is_executable(&bytes), expected, "{kind} {data}");
            }
        }
        assert!(!elf_is_executable(b"#!/bin/sh\necho\n"));
        let mut short = header(CLASS_64, DATA_LITTLE_ENDIAN, 183);
        short[16] = 2;
        assert!(!elf_is_executable(&short[..19]));
    }

    #[test]
    fn what_is_not_a_readable_elf() {
        assert_eq!(elf_arch(b"#!/bin/sh\necho\n"), None);
        assert!(!is_elf(b"#!/bin/sh\necho\n"));
        // Trop court, ou d'un boutisme inconnu.
        assert_eq!(elf_arch(&header(CLASS_64, 1, 62)[..19]), None);
        assert_eq!(elf_arch(&header(CLASS_64, 3, 62)), None);
        assert!(is_elf(b"\x7fELF"));
    }
}
