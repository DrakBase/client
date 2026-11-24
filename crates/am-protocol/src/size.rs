//! Taille d'un fichier dans les messages de l'outil et de l'agent : la même écriture des deux
//! côtés, dans les unités des limites du contrat (Kio, Mio, Gio).

/// Taille lisible, en unités binaires et à la française : `999 o`, `12,1 Kio`, `25,2 Mio`,
/// `1,0 Gio`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["Kio", "Mio", "Gio"];
    if bytes < 1024 {
        return format!("{bytes} o");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // 1023,95 s'écrirait « 1024,0 » : l'unité suivante prend le relais.
    while value >= 1023.95 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit]).replace('.', ",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_written_in_binary_units() {
        assert_eq!(human_size(0), "0 o");
        assert_eq!(human_size(999), "999 o");
        assert_eq!(human_size(1024), "1,0 Kio");
        assert_eq!(human_size(12_345), "12,1 Kio");
        assert_eq!(human_size(26_430_000), "25,2 Mio");
        assert_eq!(human_size(17 * 1024 * 1024), "17,0 Mio");
        assert_eq!(human_size(1 << 30), "1,0 Gio");
        // Juste sous une unité : jamais « 1024,0 ».
        assert_eq!(human_size(1024 * 1024 - 1), "1,0 Mio");
        assert_eq!(human_size((1 << 30) - 1), "1,0 Gio");
        assert_eq!(human_size(5 * (1 << 40)), "5120,0 Gio");
    }
}
