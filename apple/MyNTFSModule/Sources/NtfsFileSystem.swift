import FSKit
import Foundation

/// FSKit NTFS module skeleton (Phase 5 — not product-ready).
///
/// Requires paid `com.apple.developer.fskit.fsmodule` entitlement + app sandbox.
/// Normal users should use `apple/MyNTFS.app` (dedicated app) instead.
///
/// Mount (when wired): `sudo mount -F -t myntfs /dev/diskNsY /Volumes/Label`
///
/// Probe policy: do **not** recognize block devices until boot-sector NTFS OEM
/// (`"NTFS    "` at offset 3) is verified via `metadataRead` + `myntfs_probe` FFI.
@objc(NtfsFileSystem)
final class NtfsFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    func probe(resource: FSResource, reply: @escaping (FSProbeResult?, (any Error)?) -> Void) {
        guard resource is FSBlockDeviceResource else {
            reply(FSProbeResult.notRecognized, nil)
            return
        }
        // Conservative until wired to Rust probe: never outrank Apple RO NTFS by default.
        reply(FSProbeResult.notRecognized, nil)
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        reply: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        reply(nil, NSError(domain: "MyNTFS", code: 1, userInfo: [
            NSLocalizedDescriptionKey: "FSKit module not wired — use MyNTFS.app for now"
        ]))
    }
}
