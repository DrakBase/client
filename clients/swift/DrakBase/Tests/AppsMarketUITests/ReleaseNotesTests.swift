@testable import AppsMarketUI
import Foundation
import Testing

@Suite("Notes de version en Markdown")
struct ReleaseNotesTests {
    @Test("Titres, listes à puces et numérotées, paragraphes, citations, code et séparateurs")
    func blocks() {
        let markdown = """
            ## Nouveautés

            - Traduction plus rapide
            - Raccourci clavier
              sur deux lignes
              * sous-élément
            1. Premier
            2) Second

            Un paragraphe
            sur deux lignes.
            > Une citation

            ```
            let x = 1
            ```
            ---
            #pas un titre
            """
        #expect(ReleaseNotes.blocks(from: markdown) == [
            .heading(level: 2, text: "Nouveautés"),
            .bullet(text: "Traduction plus rapide", depth: 0),
            .bullet(text: "Raccourci clavier sur deux lignes", depth: 0),
            .bullet(text: "sous-élément", depth: 1),
            .numbered(marker: "1", text: "Premier", depth: 0),
            .numbered(marker: "2", text: "Second", depth: 0),
            .paragraph("Un paragraphe sur deux lignes."),
            .quote("Une citation"),
            .code("let x = 1"),
            .divider,
            .paragraph("#pas un titre"),
        ])
    }

    @Test("Fins de ligne Windows, notes vides, bloc de code non fermé")
    func edgeCases() {
        #expect(ReleaseNotes.blocks(from: "- un\r\n- deux\r\n") == [.bullet(text: "un", depth: 0), .bullet(text: "deux", depth: 0)])
        #expect(ReleaseNotes.blocks(from: "").isEmpty)
        #expect(ReleaseNotes.blocks(from: "\n\n").isEmpty)
        #expect(ReleaseNotes.blocks(from: "```\ncode") == [.code("code")])
        #expect(ReleaseNotes.blocks(from: "# Titre #") == [.heading(level: 1, text: "Titre")])
    }

    @Test("En ligne : gras et liens http(s) gardés, autres liens et images retirés")
    func inlineMarkdown() throws {
        let text = ReleaseNotes.inline("**Gras**, [site](https://example.fr), [fichier](file:///etc/passwd), [app](translate://ouvrir), ![image](https://example.fr/a.png)")
        #expect(String(text.characters).hasPrefix("Gras, site, fichier, app"))
        let links = text.runs.compactMap(\.link)
        #expect(links == [try #require(URL(string: "https://example.fr"))])
        #expect(text.runs.allSatisfy { $0.imageURL == nil })
        #expect(text.runs.contains { $0.inlinePresentationIntent?.contains(.stronglyEmphasized) == true })

        // Un texte que le lecteur refuse reste affiché tel quel.
        #expect(String(ReleaseNotes.inline("simple").characters) == "simple")
    }
}
