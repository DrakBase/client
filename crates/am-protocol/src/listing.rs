//! Fiche publique d'une app (docs/API.md, « Apps » et « Captures d'écran ») : ses limites, que le
//! serveur impose et que l'outil contrôle avant d'envoyer, et la lecture de l'en-tête d'un PNG.

use std::ops::RangeInclusive;

/// Longueur maximale de la description, en caractères.
pub const MAX_DESCRIPTION_CHARS: usize = 4000;
/// Captures d'écran par app.
pub const MAX_SCREENSHOTS: usize = 10;
/// Taille maximale d'une capture : 8 Mio.
pub const MAX_SCREENSHOT_BYTES: u64 = 8 * 1024 * 1024;
/// Côtés permis d'une capture, en pixels.
pub const SCREENSHOT_SIDES: RangeInclusive<u32> = 320..=8192;
/// Longueur maximale de la légende d'une capture, en caractères.
pub const MAX_CAPTION_CHARS: usize = 200;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Le bloc `IHDR` entier : longueur (4 octets), type (4), données (13) et CRC (4).
const IHDR_CHUNK_LEN: usize = 25;

/// Largeur et hauteur d'un PNG, lues dans son en-tête : la signature, puis le bloc `IHDR` de
/// 13 octets, dont le CRC doit être juste. `None` si ce n'est pas un PNG lisible, ou si un côté
/// vaut 0.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let chunk = bytes.strip_prefix(PNG_SIGNATURE)?.get(..IHDR_CHUNK_LEN)?;
    let word =
        |at: usize| u32::from_be_bytes([chunk[at], chunk[at + 1], chunk[at + 2], chunk[at + 3]]);
    if word(0) != 13 || &chunk[4..8] != b"IHDR" {
        return None;
    }
    let mut crc = flate2::Crc::new();
    crc.update(&chunk[4..21]);
    if crc.sum() != word(21) {
        return None;
    }
    let (width, height) = (word(8), word(12));
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// En-tête d'un PNG de cette taille : signature et bloc `IHDR`, sans les données.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend(width.to_be_bytes());
        ihdr.extend(height.to_be_bytes());
        // Profondeur 8, couleur RVBA, compression, filtre et entrelacement à 0.
        ihdr.extend([8, 6, 0, 0, 0]);
        let mut crc = flate2::Crc::new();
        crc.update(&ihdr);
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend(13u32.to_be_bytes());
        png.extend(&ihdr);
        png.extend(crc.sum().to_be_bytes());
        png
    }

    #[test]
    fn png_headers_are_read_and_checked() {
        let png = png_header(1280, 800);
        assert_eq!(png_dimensions(&png), Some((1280, 800)));
        // Ce qui suit l'en-tête n'est pas lu.
        let mut longer = png.clone();
        longer.extend(b"IDAT...");
        assert_eq!(png_dimensions(&longer), Some((1280, 800)));

        assert_eq!(png_dimensions(PNG_SIGNATURE), None);
        assert_eq!(png_dimensions(&png[..png.len() - 1]), None);
        assert_eq!(png_dimensions(b"GIF89a"), None);
        assert_eq!(png_dimensions(&png_header(0, 800)), None);
        assert_eq!(png_dimensions(&png_header(1280, 0)), None);
        // Une taille changée sans refaire le CRC.
        let mut tampered = png.clone();
        tampered[19] ^= 1;
        assert_eq!(png_dimensions(&tampered), None);
        // Un autre bloc à la place d'IHDR.
        let mut other = png;
        other[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_dimensions(&other), None);
    }

    #[test]
    fn limits_are_those_of_the_contract() {
        assert!(SCREENSHOT_SIDES.contains(&320) && SCREENSHOT_SIDES.contains(&8192));
        assert!(!SCREENSHOT_SIDES.contains(&319) && !SCREENSHOT_SIDES.contains(&8193));
        assert_eq!(MAX_SCREENSHOT_BYTES, 8_388_608);
    }
}
