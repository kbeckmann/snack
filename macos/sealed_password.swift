// Saved-password protection for macOS, linked into Snack by build.rs.
//
// The legacy login keychain trusts an unsigned build only by its exact hash,
// so every rebuild asks for the login password. Instead, encrypt the saved
// password to a Secure Enclave key that needs Touch ID (or the Mac password
// as a fallback) to use. The key never leaves the Secure Enclave; only its
// device-bound, encrypted handle is stored with the ciphertext.
//
// Sealed layout: "SNK1" | u32 LE key-handle length | key handle |
//                65-byte X9.63 ephemeral public key | AES-GCM combined box.
import CryptoKit
import Foundation
import LocalAuthentication
import Security

private let magic = Data("SNK1".utf8)
private let info = Data("snack saved password v1".utf8)

private func symmetricKey(_ secret: SharedSecret) -> SymmetricKey {
    secret.hkdfDerivedSymmetricKey(using: SHA256.self, salt: Data(), sharedInfo: info, outputByteCount: 32)
}

private func copyOut(_ data: Data, _ output: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
                     _ length: UnsafeMutablePointer<Int>) {
    let buffer = malloc(max(data.count, 1))!.assumingMemoryBound(to: UInt8.self)
    data.copyBytes(to: buffer, count: data.count)
    output.pointee = buffer
    length.pointee = data.count
}

private func describe(_ error: Error) -> Int32 {
    let error = error as NSError
    // User cancelled: LocalAuthentication, or Security's errSecUserCanceled.
    if (error.domain == LAError.errorDomain && [LAError.userCancel.rawValue, LAError.appCancel.rawValue,
        LAError.systemCancel.rawValue].contains(error.code)) || error.code == Int(errSecUserCanceled) {
        return 1
    }
    return 2
}

private func message(_ error: Error, _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) {
    out.pointee = strdup("\(error.localizedDescription)")
}

/// Encrypt `input` to a new Secure Enclave key. Returns 0 on success.
@_cdecl("snack_seal_password")
public func sealPassword(_ input: UnsafePointer<UInt8>, _ inputLength: Int,
                         _ output: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
                         _ outputLength: UnsafeMutablePointer<Int>,
                         _ error: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32 {
    do {
        var accessError: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                                                           [.privateKeyUsage, .userPresence], &accessError) else {
            throw accessError!.takeRetainedValue() as Error
        }
        let key = try SecureEnclave.P256.KeyAgreement.PrivateKey(accessControl: access)
        let ephemeral = P256.KeyAgreement.PrivateKey()
        let secret = try ephemeral.sharedSecretFromKeyAgreement(with: key.publicKey)
        let box = try AES.GCM.seal(Data(bytes: input, count: inputLength), using: symmetricKey(secret))
        guard let combined = box.combined else { throw CryptoKitError.incorrectParameterSize }
        var sealed = magic
        var handleLength = UInt32(key.dataRepresentation.count).littleEndian
        sealed.append(Data(bytes: &handleLength, count: 4))
        sealed.append(key.dataRepresentation)
        sealed.append(ephemeral.publicKey.x963Representation)
        sealed.append(combined)
        copyOut(sealed, output, outputLength)
        return 0
    } catch let failure {
        message(failure, error)
        return 2
    }
}

/// Decrypt sealed bytes; asks for Touch ID (or the Mac password). Returns 0 on
/// success, 1 when the user cancelled, 2 on other failures.
@_cdecl("snack_open_password")
public func openPassword(_ input: UnsafePointer<UInt8>, _ inputLength: Int,
                         _ reason: UnsafePointer<CChar>,
                         _ output: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>,
                         _ outputLength: UnsafeMutablePointer<Int>,
                         _ error: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32 {
    do {
        let sealed = Data(bytes: input, count: inputLength)
        guard sealed.count > 8, sealed.prefix(4) == magic else { throw CryptoKitError.incorrectParameterSize }
        let handleLength = Int(sealed.subdata(in: 4..<8).withUnsafeBytes { $0.loadUnaligned(as: UInt32.self) }.littleEndian)
        let handleEnd = 8 + handleLength
        guard sealed.count > handleEnd + 65 else { throw CryptoKitError.incorrectParameterSize }
        let context = LAContext()
        context.localizedReason = String(cString: reason)
        let key = try SecureEnclave.P256.KeyAgreement.PrivateKey(
            dataRepresentation: sealed.subdata(in: 8..<handleEnd), authenticationContext: context)
        let ephemeral = try P256.KeyAgreement.PublicKey(x963Representation: sealed.subdata(in: handleEnd..<(handleEnd + 65)))
        let secret = try key.sharedSecretFromKeyAgreement(with: ephemeral)
        let box = try AES.GCM.SealedBox(combined: sealed.subdata(in: (handleEnd + 65)..<sealed.count))
        copyOut(try AES.GCM.open(box, using: symmetricKey(secret)), output, outputLength)
        return 0
    } catch let failure {
        message(failure, error)
        return describe(failure)
    }
}

@_cdecl("snack_free")
public func snackFree(_ pointer: UnsafeMutableRawPointer?) {
    free(pointer)
}
