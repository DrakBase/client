//! Texte venu d'une stack ou du serveur, avant de partir au terminal ou dans un fichier que lit une
//! interface : ce qu'un terminal exécuterait ou ne montrerait pas y est rendu visible. Une valeur
//! comme `x\u{1b}[1A\u{1b}[2K` remonterait sinon le curseur et effacerait la ligne d'avant.

/// Le caractère est-il de ceux qu'un terminal exécute, ou que personne ne voit ? Les caractères de
/// contrôle (C0, DEL, C1, saut de ligne compris), les séparateurs de ligne et de paragraphe, les
/// marques de sens d'écriture et les caractères invisibles.
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// Le texte contient-il un caractère caché ([`is_hidden`]) ?
pub fn has_hidden(text: &str) -> bool {
    text.chars().any(is_hidden)
}

/// Le texte, sur une ligne, ses caractères cachés écrits en clair : `\n`, `\r`, `\t`, et
/// `\u{1b}` pour les autres. Un texte sans caractère caché revient tel quel.
pub fn visible(text: &str) -> String {
    escape(text, false)
}

/// Comme [`visible`], pour un texte de plusieurs lignes (des notes de version) : ses sauts de
/// ligne restent des sauts de ligne.
pub fn visible_lines(text: &str) -> String {
    escape(text, true)
}

fn escape(text: &str, keep_line_feeds: bool) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' if keep_line_feeds => shown.push('\n'),
            '\n' => shown.push_str("\\n"),
            '\r' => shown.push_str("\\r"),
            '\t' => shown.push_str("\\t"),
            hidden if is_hidden(hidden) => {
                shown.push_str(&format!("\\u{{{:x}}}", u32::from(hidden)));
            }
            plain => shown.push(plain),
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_characters_are_written_out() {
        assert_eq!(visible("port 8080/tcp"), "port 8080/tcp");
        assert_eq!(visible("réseau de l'hôte : été"), "réseau de l'hôte : été");
        // Remonter d'une ligne, l'effacer, revenir au début : la séquence s'affiche, sans agir.
        assert_eq!(
            visible("x\u{1b}[1A\u{1b}[2K\r  - app : port 8080"),
            "x\\u{1b}[1A\\u{1b}[2K\\r  - app : port 8080"
        );
        assert_eq!(visible("a\nb\tc\u{7f}\u{9b}"), "a\\nb\\tc\\u{7f}\\u{9b}");
        // Sens d'écriture inversé, espace sans largeur, séparateur de ligne, marque d'ordre.
        assert_eq!(
            visible("a\u{202e}b\u{200b}c\u{2028}d\u{2066}e\u{feff}"),
            "a\\u{202e}b\\u{200b}c\\u{2028}d\\u{2066}e\\u{feff}"
        );
        assert!(has_hidden("a\u{200f}") && !has_hidden("a b—c"));
    }

    #[test]
    fn notes_keep_their_lines() {
        assert_eq!(
            visible_lines("- Un\n- Deux\u{1b}[2J\r\n"),
            "- Un\n- Deux\\u{1b}[2J\\r\n"
        );
    }
}
