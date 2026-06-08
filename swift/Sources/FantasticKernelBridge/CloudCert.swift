// Hand-built Ed25519 self-signed X.509 cert for cloud_bridge (RFC 8410).
//
// No Swift library signs an X.509 cert with an Ed25519 key (swift-certificates
// supports P-256/RSA issuers only), so we encode the minimal DER ourselves and
// sign the TBSCertificate with `Crypto`'s Ed25519 (CryptoKit on Apple, portable).
//
// The cert is a DISPOSABLE CARRIER of the device's durable identity — its
// Ed25519 PUBLIC KEY. Peers pin that key (not the cert bytes), so the signature
// need not be deterministic: Apple's CryptoKit randomizes Ed25519 signatures
// (fault-attack hedging), which is fine — the pubkey is stable, the cert can
// rotate freely. (See CloudBridge.swift's pubkey-pinning verify callback.)
//
// Structure: a v3 cert with serial 1, issuer == subject (CN = b64url(pubkey)),
// validity 2020-01-01 … 2049-12-31 (UTCTime), an Ed25519 SubjectPublicKeyInfo,
// and a critical BasicConstraints CA:TRUE.

import Crypto
import Foundation

public enum CloudCert {
    // ── minimal DER ────────────────────────────────────────────────

    /// DER length octets (short form < 128, else long form).
    private static func derLen(_ n: Int) -> [UInt8] {
        if n < 0x80 { return [UInt8(n)] }
        var len = n
        var bytes: [UInt8] = []
        while len > 0 {
            bytes.insert(UInt8(len & 0xFF), at: 0)
            len >>= 8
        }
        return [UInt8(0x80 | bytes.count)] + bytes
    }

    /// A tag-length-value triple.
    private static func tlv(_ tag: UInt8, _ content: [UInt8]) -> [UInt8] {
        [tag] + derLen(content.count) + content
    }

    private static func seq(_ items: [[UInt8]]) -> [UInt8] { tlv(0x30, items.flatMap { $0 }) }
    private static func set(_ items: [[UInt8]]) -> [UInt8] { tlv(0x31, items.flatMap { $0 }) }
    private static func integer(_ bytes: [UInt8]) -> [UInt8] { tlv(0x02, bytes) }
    private static func utf8(_ s: String) -> [UInt8] { tlv(0x0C, Array(s.utf8)) }
    private static func utcTime(_ s: String) -> [UInt8] { tlv(0x17, Array(s.utf8)) }
    private static func bitString(_ bytes: [UInt8]) -> [UInt8] { tlv(0x03, [0x00] + bytes) }
    private static func octetString(_ bytes: [UInt8]) -> [UInt8] { tlv(0x04, bytes) }
    private static func boolean(_ v: Bool) -> [UInt8] { tlv(0x01, [v ? 0xFF : 0x00]) }
    private static func explicit(_ tag: UInt8, _ content: [UInt8]) -> [UInt8] { tlv(tag, content) }

    // OIDs (pre-encoded OID contents).
    private static let oidEd25519: [UInt8] = [0x06, 0x03, 0x2B, 0x65, 0x70] // 1.3.101.112
    private static let oidCommonName: [UInt8] = [0x06, 0x03, 0x55, 0x04, 0x03] // 2.5.4.3
    private static let oidBasicConstraints: [UInt8] = [0x06, 0x03, 0x55, 0x1D, 0x13] // 2.5.29.19

    /// AlgorithmIdentifier for Ed25519 (no parameters).
    private static var algEd25519: [UInt8] { seq([oidEd25519]) }

    /// A Name with a single CN RDN.
    private static func name(cn: String) -> [UInt8] {
        seq([set([seq([oidCommonName, utf8(cn)])])])
    }

    /// PKCS8 DER wrapping a raw 32-byte Ed25519 seed (what NIOSSL loads as the key).
    public static func ed25519PKCS8(_ seed: [UInt8]) -> [UInt8] {
        [0x30, 0x2E, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2B, 0x65, 0x70,
         0x04, 0x22, 0x04, 0x20] + seed
    }

    /// b64url-nopad (matches the Python/Rust CN convention).
    public static func b64url(_ bytes: [UInt8]) -> String {
        Data(bytes).base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    /// Decode a b64url-nopad string (e.g. the device `id_key` from the agent
    /// record) back to bytes. Returns nil on malformed input.
    public static func b64urlDecode(_ s: String) -> [UInt8]? {
        var b64 =
            s
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while b64.count % 4 != 0 { b64.append("=") }
        guard let data = Data(base64Encoded: b64) else { return nil }
        return [UInt8](data)
    }

    /// Standard PEM wrapping of a cert DER (so a peer can pin it as
    /// `approved_peer_certs`, and the relay e2e harness can collect a swift leg's
    /// ACTUAL cert via the `__cloud-cert` CLI subcommand).
    public static func pem(_ der: [UInt8]) -> String {
        let b64 = Data(der).base64EncodedString()
        var s = "-----BEGIN CERTIFICATE-----\n"
        var i = b64.startIndex
        while i < b64.endIndex {
            let j = b64.index(i, offsetBy: 64, limitedBy: b64.endIndex) ?? b64.endIndex
            s += b64[i..<j] + "\n"
            i = j
        }
        s += "-----END CERTIFICATE-----\n"
        return s
    }

    /// A deterministic self-signed Ed25519 cert (DER) + the key PKCS8 DER, from a
    /// 32-byte device identity seed.
    public static func selfSigned(idKey: [UInt8]) throws -> (certDER: [UInt8], keyPKCS8: [UInt8]) {
        let priv = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(idKey))
        let pub = [UInt8](priv.publicKey.rawRepresentation)
        let cn = b64url(pub)

        let spki = seq([algEd25519, bitString(pub)])
        let validity = seq([utcTime("200101000000Z"), utcTime("491231235959Z")])
        let basicConstraints = seq([
            oidBasicConstraints, boolean(true), octetString(seq([boolean(true)])),
        ])
        let extensions = explicit(0xA3, seq([basicConstraints]))

        let tbs = seq([
            explicit(0xA0, integer([0x02])), // version v3
            integer([0x01]), // serial
            algEd25519, // signature alg
            name(cn: cn), // issuer
            validity,
            name(cn: cn), // subject (self-signed)
            spki,
            extensions,
        ])

        let sig = try priv.signature(for: Data(tbs))
        let cert = seq([tbs, algEd25519, bitString([UInt8](sig))])
        return (cert, ed25519PKCS8(idKey))
    }
}
