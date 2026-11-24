import AppsMarketCrypto
import AppsMarketTestSupport
import Foundation
import Testing

@Suite("BLAKE2b-512")
struct BLAKE2bTests {
    @Test("Les vecteurs de fixtures/blake2b.json, dont celui de la RFC 7693")
    func fixtureVectors() throws {
        let fixtures = try Fixtures.blake2b()
        #expect(fixtures.algorithm == "BLAKE2b-512")
        #expect(fixtures.vectors.contains { $0.text == "abc" })
        for vector in fixtures.vectors {
            #expect(BLAKE2b512.hash(data: vector.input).hexString == vector.digest, "\(vector.label)")
        }
    }

    @Test("Le vecteur « abc » de la RFC 7693, annexe A")
    func rfcVector() {
        let digest = BLAKE2b512.hash(data: Array("abc".utf8))
        #expect(digest.count == BLAKE2b512.byteCount)
        #expect(digest.hexString == "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923")
    }

    @Test("Par morceaux de toute taille, l'empreinte est la même qu'en une fois")
    func chunkSizes() {
        let data = Fixtures.pattern(length: 65_537)
        let expected = BLAKE2b512.hash(data: data)
        for chunkSize in [1, 7, 63, 64, 127, 128, 129, 255, 256, 1000, 4096, 65_536, 65_537] {
            var hasher = BLAKE2b512()
            for start in stride(from: 0, to: data.count, by: chunkSize) {
                hasher.update(data: data[start..<min(start + chunkSize, data.count)])
            }
            #expect(hasher.finalize() == expected, "morceaux de \(chunkSize) octets")
        }
    }

    @Test("Coupé en deux à n'importe quel endroit, autour des frontières de blocs")
    func everySplitPoint() {
        let data = Fixtures.pattern(length: 600)
        let expected = BLAKE2b512.hash(data: data)
        for split in 0...data.count {
            var hasher = BLAKE2b512()
            hasher.update(data: data[..<split])
            hasher.update(data: data[split...])
            #expect(hasher.finalize() == expected, "coupure à \(split)")
        }
    }

    @Test("finalize() laisse le calcul continuer, et les morceaux vides ne changent rien")
    func finalizeKeepsState() {
        var hasher = BLAKE2b512()
        hasher.update(data: Array("ab".utf8))
        let partial = hasher.finalize()
        hasher.update(data: [UInt8]())
        hasher.update(bufferPointer: UnsafeRawBufferPointer(start: nil, count: 0))
        #expect(hasher.finalize() == partial)
        #expect(partial == BLAKE2b512.hash(data: Array("ab".utf8)))

        hasher.update(data: Array("c".utf8))
        #expect(hasher.finalize() == BLAKE2b512.hash(data: Array("abc".utf8)))
    }

    @Test("Data, tableaux et pointeurs bruts donnent la même empreinte")
    func inputKinds() {
        let bytes = Fixtures.pattern(length: 1000)
        var fromPointer = BLAKE2b512()
        bytes.withUnsafeBytes { fromPointer.update(bufferPointer: $0) }
        #expect(BLAKE2b512.hash(data: Data(bytes)) == BLAKE2b512.hash(data: bytes))
        #expect(fromPointer.finalize() == BLAKE2b512.hash(data: bytes))
    }
}
