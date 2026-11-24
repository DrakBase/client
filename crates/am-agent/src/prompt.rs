//! Questions posées dans le terminal, seulement en mode interactif.

use std::io::{self, BufRead, Write};

/// Pose une question fermée ; seuls « o », « oui », « y » et « yes » valent accord.
pub fn confirm(question: &str) -> io::Result<bool> {
    let answer = ask(&format!("{question} [o/N] "))?;
    Ok(matches!(
        answer.to_lowercase().as_str(),
        "o" | "oui" | "y" | "yes"
    ))
}

/// Pose une question et rend la réponse, sans blancs autour.
pub fn ask(question: &str) -> io::Result<String> {
    let mut stderr = io::stderr();
    write!(stderr, "{question}")?;
    stderr.flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim().to_owned())
}
