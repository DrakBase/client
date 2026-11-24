//! Saisie des mots de passe et du jeton : au terminal, sans écho, ou une ligne par secret sur
//! l'entrée standard, pour les scripts et les tests. Et une réponse ordinaire, tapée au terminal.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};

use anyhow::{Context, Result, anyhow, bail};

pub trait PasswordSource {
    /// Lit un mot de passe ; `prompt` ne s'affiche qu'au terminal.
    fn read(&mut self, prompt: &str) -> Result<String>;

    /// Vrai au terminal, où une saisie ratée peut se refaire ; faux pour l'entrée standard, qui ne
    /// donne chaque ligne qu'une fois.
    fn interactive(&self) -> bool;
}

/// Le terminal (`/dev/tty`), même quand l'entrée standard est redirigée.
pub struct Terminal {
    /// Ce qui est demandé, pour le message d'erreur : « le mot de passe », « le jeton ».
    pub secret: &'static str,
    /// Ce qu'il faut faire sans terminal, pour le message d'erreur.
    pub otherwise: &'static str,
}

impl PasswordSource for Terminal {
    fn read(&mut self, prompt: &str) -> Result<String> {
        // rpassword ouvre le même fichier : sans terminal, on le dit avant d'afficher l'invite.
        if OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_err()
        {
            bail!(
                "pas de terminal pour saisir {} ; {}",
                self.secret,
                self.otherwise
            );
        }
        rpassword::prompt_password(prompt)
            .with_context(|| format!("saisie de {} au terminal", self.secret))
    }

    fn interactive(&self) -> bool {
        true
    }
}

/// Une ligne tapée au terminal, avec écho. Sans terminal, l'erreur dit quoi faire : « pas de
/// terminal pour demander `what` : `otherwise` ».
pub fn ask_line(prompt: &str, what: &str, otherwise: &str) -> Result<String> {
    let terminal = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| anyhow!("pas de terminal pour demander {what} : {otherwise}"))?;
    let mut output = &terminal;
    output.write_all(prompt.as_bytes())?;
    output.flush()?;
    let mut line = String::new();
    BufReader::new(&terminal)
        .read_line(&mut line)
        .context("lecture au terminal")?;
    Ok(line.trim().to_owned())
}

/// Un mot de passe par ligne, lu une seule fois.
pub struct Lines<R>(pub R);

impl<R: BufRead> PasswordSource for Lines<R> {
    fn read(&mut self, _prompt: &str) -> Result<String> {
        let mut line = String::new();
        if self
            .0
            .read_line(&mut line)
            .context("lecture de l'entrée standard")?
            == 0
        {
            bail!("l'entrée standard s'arrête avant le mot de passe attendu");
        }
        let line = line.strip_suffix('\n').unwrap_or(&line);
        Ok(line.strip_suffix('\r').unwrap_or(line).to_owned())
    }

    fn interactive(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_strip_line_endings_and_stop_at_the_end() {
        let mut source = Lines(&b"premier mot\nsecond\r\n"[..]);
        assert_eq!(source.read("").unwrap(), "premier mot");
        assert_eq!(source.read("").unwrap(), "second");
        assert!(source.read("").is_err());
        assert!(!source.interactive());
    }
}
