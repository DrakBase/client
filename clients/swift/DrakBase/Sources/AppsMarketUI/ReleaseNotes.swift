import Foundation
import SwiftUI

/// Bloc des notes de version. `Text` ne sait afficher que le Markdown en ligne : les titres, les
/// listes et le code sont découpés ici, ligne par ligne.
enum ReleaseNoteBlock: Equatable {
    case heading(level: Int, text: String)
    case bullet(text: String, depth: Int)
    case numbered(marker: String, text: String, depth: Int)
    case paragraph(String)
    case quote(String)
    case code(String)
    case divider
}

/// Lecture des notes de version en Markdown, sans HTML (`docs/PLAN.md`, « En-têtes HTTP »).
enum ReleaseNotes {
    static func blocks(from markdown: String) -> [ReleaseNoteBlock] {
        var blocks: [ReleaseNoteBlock] = []
        var paragraph: [String] = []
        var paragraphIsQuote = false
        var code: [String]?

        func flushParagraph() {
            if !paragraph.isEmpty {
                let text = paragraph.joined(separator: " ")
                blocks.append(paragraphIsQuote ? .quote(text) : .paragraph(text))
                paragraph = []
            }
            paragraphIsQuote = false
        }

        // `\r\n` est un seul caractère pour Swift : le découpage se fait sur toute fin de ligne.
        for rawLine in markdown.split(omittingEmptySubsequences: false, whereSeparator: \.isNewline) {
            let line = String(rawLine)
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("```") {
                if let lines = code {
                    blocks.append(.code(lines.joined(separator: "\n")))
                    code = nil
                } else {
                    flushParagraph()
                    code = []
                }
                continue
            }
            if code != nil {
                code?.append(line)
                continue
            }
            if trimmed.isEmpty {
                flushParagraph()
                continue
            }
            if let heading = heading(trimmed) {
                flushParagraph()
                blocks.append(heading)
            } else if isDivider(trimmed) {
                flushParagraph()
                blocks.append(.divider)
            } else if let item = listItem(line) {
                flushParagraph()
                blocks.append(item)
            } else if line.first?.isWhitespace == true, paragraph.isEmpty, let last = blocks.last, let continued = continuing(last, with: trimmed) {
                // Suite d'un élément de liste, sur une ligne indentée.
                blocks[blocks.count - 1] = continued
            } else {
                // Une citation (`> texte`) fait son propre paragraphe.
                let isQuote = trimmed.hasPrefix(">")
                if isQuote != paragraphIsQuote {
                    flushParagraph()
                }
                paragraphIsQuote = isQuote
                paragraph.append(isQuote ? String(trimmed.dropFirst()).trimmingCharacters(in: .whitespaces) : trimmed)
            }
        }
        if let lines = code {
            blocks.append(.code(lines.joined(separator: "\n")))
        }
        flushParagraph()
        return blocks
    }

    /// Markdown en ligne : gras, italique, code et liens. Seuls les liens http(s) et mailto restent
    /// cliquables : les notes viennent du serveur.
    static func inline(_ text: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        guard var attributed = try? AttributedString(markdown: text, options: options) else {
            return AttributedString(text)
        }
        for run in attributed.runs {
            if let link = run.link, !["http", "https", "mailto"].contains(link.scheme?.lowercased() ?? "") {
                attributed[run.range].link = nil
            }
            if run.imageURL != nil {
                attributed[run.range].imageURL = nil
            }
        }
        return attributed
    }

    private static func heading(_ line: String) -> ReleaseNoteBlock? {
        let level = line.prefix { $0 == "#" }.count
        guard (1...6).contains(level) else { return nil }
        let rest = line.dropFirst(level)
        guard rest.isEmpty || rest.first == " " else { return nil }
        var text = rest.trimmingCharacters(in: .whitespaces)
        while text.hasSuffix("#") {
            text.removeLast()
        }
        return .heading(level: level, text: text.trimmingCharacters(in: .whitespaces))
    }

    private static func isDivider(_ line: String) -> Bool {
        let characters = line.filter { $0 != " " }
        guard characters.count >= 3, let first = characters.first, "-*_".contains(first) else { return false }
        return characters.allSatisfy { $0 == first }
    }

    /// `- texte`, `* texte`, `+ texte`, `1. texte` ou `1) texte` ; deux espaces d'indentation font un
    /// niveau.
    private static func listItem(_ line: String) -> ReleaseNoteBlock? {
        let indentation = line.prefix { $0 == " " || $0 == "\t" }
        let depth = indentation.reduce(0) { $0 + ($1 == "\t" ? 4 : 1) } / 2
        let rest = line.dropFirst(indentation.count)
        if let marker = rest.first, "-*+".contains(marker), rest.dropFirst().first == " " {
            return .bullet(text: rest.dropFirst(2).trimmingCharacters(in: .whitespaces), depth: depth)
        }
        let digits = rest.prefix { $0.isASCII && $0.isNumber }
        guard (1...9).contains(digits.count) else { return nil }
        let afterDigits = rest.dropFirst(digits.count)
        guard let delimiter = afterDigits.first, delimiter == "." || delimiter == ")", afterDigits.dropFirst().first == " " else {
            return nil
        }
        return .numbered(marker: String(digits), text: afterDigits.dropFirst(2).trimmingCharacters(in: .whitespaces), depth: depth)
    }

    private static func continuing(_ block: ReleaseNoteBlock, with text: String) -> ReleaseNoteBlock? {
        switch block {
        case .bullet(let item, let depth):
            .bullet(text: item + " " + text, depth: depth)
        case .numbered(let marker, let item, let depth):
            .numbered(marker: marker, text: item + " " + text, depth: depth)
        default:
            nil
        }
    }
}

/// Notes de version, dans une zone qui défile.
struct ReleaseNotesView: View {
    private let blocks: [ReleaseNoteBlock]

    init(markdown: String) {
        blocks = ReleaseNotes.blocks(from: markdown)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 6) {
                ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                    row(block)
                }
            }
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
        }
        .background(Color(nsColor: .textBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 6))
        .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(nsColor: .separatorColor)))
    }

    @ViewBuilder
    private func row(_ block: ReleaseNoteBlock) -> some View {
        switch block {
        case .heading(let level, let text):
            Text(ReleaseNotes.inline(text))
                .font(level <= 2 ? .headline : .subheadline.bold())
                .padding(.top, 4)
        case .bullet(let text, let depth):
            item(marker: "•", text: text, depth: depth)
        case .numbered(let marker, let text, let depth):
            item(marker: "\(marker).", text: text, depth: depth)
        case .paragraph(let text):
            Text(ReleaseNotes.inline(text))
        case .quote(let text):
            Text(ReleaseNotes.inline(text))
                .foregroundStyle(.secondary)
                .padding(.leading, 10)
                .overlay(alignment: .leading) {
                    Rectangle()
                        .fill(Color(nsColor: .separatorColor))
                        .frame(width: 3)
                }
        case .code(let text):
            Text(text)
                .font(.system(.body, design: .monospaced))
                .padding(6)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color(nsColor: .quaternaryLabelColor).opacity(0.3), in: RoundedRectangle(cornerRadius: 4))
        case .divider:
            Divider()
        }
    }

    private func item(marker: String, text: String, depth: Int) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text(marker)
                .monospacedDigit()
            Text(ReleaseNotes.inline(text))
        }
        .padding(.leading, CGFloat(depth) * 16)
    }
}
