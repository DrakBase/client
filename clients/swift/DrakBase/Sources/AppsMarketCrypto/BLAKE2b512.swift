import Foundation

/// BLAKE2b-512 sans clé (RFC 7693), calculé en flux : c'est l'empreinte du fichier que signe
/// minisign en mode préhaché.
///
/// Comme les fonctions de hachage de CryptoKit : `update` autant de fois que nécessaire, puis
/// `finalize()`, qui ne modifie pas le calcul en cours.
public struct BLAKE2b512: Sendable {
    /// Taille de l'empreinte, en octets.
    public static let byteCount = 64

    private static let blockByteCount = 128

    private typealias Words = (UInt64, UInt64, UInt64, UInt64, UInt64, UInt64, UInt64, UInt64)
    private typealias Vector = (
        UInt64, UInt64, UInt64, UInt64, UInt64, UInt64, UInt64, UInt64,
        UInt64, UInt64, UInt64, UInt64, UInt64, UInt64, UInt64, UInt64
    )

    /// Vecteur d'initialisation, celui de SHA-512 (RFC 7693, section 2.6).
    private static let iv: Words = (
        0x6A09_E667_F3BC_C908, 0xBB67_AE85_84CA_A73B, 0x3C6E_F372_FE94_F82B, 0xA54F_F53A_5F1D_36F1,
        0x510E_527F_ADE6_82D1, 0x9B05_688C_2B3E_6C1F, 0x1F83_D9AB_FB41_BD6B, 0x5BE0_CD19_137E_2179
    )

    private var h: Words
    /// Octets reçus mais pas encore compressés. Le dernier bloc ne l'est qu'à la fin, avec
    /// l'indicateur de fin : il reste donc toujours ici, même plein.
    private var pending: Vector = (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
    private var pendingCount = 0
    /// Nombre d'octets compressés, sur 128 bits : `t0 + t1 × 2⁶⁴`.
    private var t0: UInt64 = 0
    private var t1: UInt64 = 0

    public init() {
        h = Self.iv
        // Bloc de paramètres : empreinte de 64 octets, sans clé, fanout et profondeur à 1.
        h.0 ^= 0x0101_0000 ^ UInt64(Self.byteCount)
    }

    /// Empreinte de données en mémoire.
    public static func hash(data: some DataProtocol) -> [UInt8] {
        var hasher = BLAKE2b512()
        hasher.update(data: data)
        return hasher.finalize()
    }

    public mutating func update(data: some DataProtocol) {
        for region in data.regions {
            region.withUnsafeBytes { update(bufferPointer: $0) }
        }
    }

    public mutating func update(bufferPointer: UnsafeRawBufferPointer) {
        guard var input = bufferPointer.baseAddress else { return }
        var remaining = bufferPointer.count
        while remaining > 0 {
            if pendingCount == Self.blockByteCount {
                // D'autres octets arrivent : le bloc en attente n'était pas le dernier.
                advanceCounter(by: Self.blockByteCount)
                let block = pending
                withUnsafeBytes(of: block) { Self.compress(&h, block: $0.baseAddress!, t0: t0, t1: t1, isLast: false) }
                pendingCount = 0
            }
            if pendingCount == 0 {
                // Les blocs entiers sont lus en place, sans copie, sauf le dernier, gardé en attente.
                while remaining > Self.blockByteCount {
                    advanceCounter(by: Self.blockByteCount)
                    Self.compress(&h, block: input, t0: t0, t1: t1, isLast: false)
                    input += Self.blockByteCount
                    remaining -= Self.blockByteCount
                }
            }
            let start = pendingCount
            let count = min(Self.blockByteCount - start, remaining)
            let source = input
            withUnsafeMutableBytes(of: &pending) { $0.baseAddress!.advanced(by: start).copyMemory(from: source, byteCount: count) }
            pendingCount += count
            input += count
            remaining -= count
        }
    }

    /// Empreinte de tout ce qui a été reçu. Le calcul peut ensuite continuer.
    public func finalize() -> [UInt8] {
        var copy = self
        return copy.finish()
    }

    private mutating func finish() -> [UInt8] {
        advanceCounter(by: pendingCount)
        let start = pendingCount
        withUnsafeMutableBytes(of: &pending) { block in
            for index in start..<Self.blockByteCount {
                block[index] = 0
            }
        }
        let block = pending
        withUnsafeBytes(of: block) { Self.compress(&h, block: $0.baseAddress!, t0: t0, t1: t1, isLast: true) }

        var digest: [UInt8] = []
        digest.reserveCapacity(Self.byteCount)
        for word in [h.0, h.1, h.2, h.3, h.4, h.5, h.6, h.7] {
            withUnsafeBytes(of: word.littleEndian) { digest.append(contentsOf: $0) }
        }
        return digest
    }

    private mutating func advanceCounter(by byteCount: Int) {
        let (sum, overflow) = t0.addingReportingOverflow(UInt64(byteCount))
        t0 = sum
        if overflow {
            t1 &+= 1
        }
    }

    /// Fonction de compression F (RFC 7693, section 3.2) sur un bloc de 128 octets.
    private static func compress(_ h: inout Words, block: UnsafeRawPointer, t0: UInt64, t1: UInt64, isLast: Bool) {
        let m: Vector = (
            word(block, 0), word(block, 1), word(block, 2), word(block, 3),
            word(block, 4), word(block, 5), word(block, 6), word(block, 7),
            word(block, 8), word(block, 9), word(block, 10), word(block, 11),
            word(block, 12), word(block, 13), word(block, 14), word(block, 15)
        )
        var v: Vector = (
            h.0, h.1, h.2, h.3, h.4, h.5, h.6, h.7,
            iv.0, iv.1, iv.2, iv.3, iv.4 ^ t0, iv.5 ^ t1, isLast ? ~iv.6 : iv.6, iv.7
        )
        // Douze tours ; chacun range les mots du bloc selon une ligne de la table SIGMA (section 2.7),
        // et les deux derniers reprennent les deux premières lignes.
        round(&v, (m.0, m.1, m.2, m.3, m.4, m.5, m.6, m.7, m.8, m.9, m.10, m.11, m.12, m.13, m.14, m.15))
        round(&v, (m.14, m.10, m.4, m.8, m.9, m.15, m.13, m.6, m.1, m.12, m.0, m.2, m.11, m.7, m.5, m.3))
        round(&v, (m.11, m.8, m.12, m.0, m.5, m.2, m.15, m.13, m.10, m.14, m.3, m.6, m.7, m.1, m.9, m.4))
        round(&v, (m.7, m.9, m.3, m.1, m.13, m.12, m.11, m.14, m.2, m.6, m.5, m.10, m.4, m.0, m.15, m.8))
        round(&v, (m.9, m.0, m.5, m.7, m.2, m.4, m.10, m.15, m.14, m.1, m.11, m.12, m.6, m.8, m.3, m.13))
        round(&v, (m.2, m.12, m.6, m.10, m.0, m.11, m.8, m.3, m.4, m.13, m.7, m.5, m.15, m.14, m.1, m.9))
        round(&v, (m.12, m.5, m.1, m.15, m.14, m.13, m.4, m.10, m.0, m.7, m.6, m.3, m.9, m.2, m.8, m.11))
        round(&v, (m.13, m.11, m.7, m.14, m.12, m.1, m.3, m.9, m.5, m.0, m.15, m.4, m.8, m.6, m.2, m.10))
        round(&v, (m.6, m.15, m.14, m.9, m.11, m.3, m.0, m.8, m.12, m.2, m.13, m.7, m.1, m.4, m.10, m.5))
        round(&v, (m.10, m.2, m.8, m.4, m.7, m.6, m.1, m.5, m.15, m.11, m.9, m.14, m.3, m.12, m.13, m.0))
        round(&v, (m.0, m.1, m.2, m.3, m.4, m.5, m.6, m.7, m.8, m.9, m.10, m.11, m.12, m.13, m.14, m.15))
        round(&v, (m.14, m.10, m.4, m.8, m.9, m.15, m.13, m.6, m.1, m.12, m.0, m.2, m.11, m.7, m.5, m.3))

        h.0 ^= v.0 ^ v.8
        h.1 ^= v.1 ^ v.9
        h.2 ^= v.2 ^ v.10
        h.3 ^= v.3 ^ v.11
        h.4 ^= v.4 ^ v.12
        h.5 ^= v.5 ^ v.13
        h.6 ^= v.6 ^ v.14
        h.7 ^= v.7 ^ v.15
    }

    /// Un tour : `G` sur les quatre colonnes, puis sur les quatre diagonales de la matrice `v`.
    /// `x` contient les mots du bloc déjà rangés selon SIGMA.
    @inline(__always)
    private static func round(_ v: inout Vector, _ x: Vector) {
        mix(&v.0, &v.4, &v.8, &v.12, x.0, x.1)
        mix(&v.1, &v.5, &v.9, &v.13, x.2, x.3)
        mix(&v.2, &v.6, &v.10, &v.14, x.4, x.5)
        mix(&v.3, &v.7, &v.11, &v.15, x.6, x.7)
        mix(&v.0, &v.5, &v.10, &v.15, x.8, x.9)
        mix(&v.1, &v.6, &v.11, &v.12, x.10, x.11)
        mix(&v.2, &v.7, &v.8, &v.13, x.12, x.13)
        mix(&v.3, &v.4, &v.9, &v.14, x.14, x.15)
    }

    /// Fonction de mélange G (RFC 7693, section 3.1), avec les rotations 32, 24, 16 et 63.
    @inline(__always)
    private static func mix(
        _ a: inout UInt64, _ b: inout UInt64, _ c: inout UInt64, _ d: inout UInt64, _ x: UInt64, _ y: UInt64
    ) {
        a = a &+ b &+ x
        d = rotateRight(d ^ a, by: 32)
        c = c &+ d
        b = rotateRight(b ^ c, by: 24)
        a = a &+ b &+ y
        d = rotateRight(d ^ a, by: 16)
        c = c &+ d
        b = rotateRight(b ^ c, by: 63)
    }

    @inline(__always)
    private static func rotateRight(_ value: UInt64, by count: UInt64) -> UInt64 {
        (value &>> count) | (value &<< (64 &- count))
    }

    /// Mot `index` du bloc, lu en petit-boutiste.
    @inline(__always)
    private static func word(_ block: UnsafeRawPointer, _ index: Int) -> UInt64 {
        UInt64(littleEndian: block.loadUnaligned(fromByteOffset: index &* 8, as: UInt64.self))
    }
}
