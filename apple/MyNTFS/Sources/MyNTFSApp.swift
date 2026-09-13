import SwiftUI
import UniformTypeIdentifiers
import Darwin
import AppKit

@main
struct MyNTFSApp: App {
    @StateObject private var model = VolumeModel()
    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
        }
        .defaultSize(width: 1100, height: 680)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("Refresh Disks") { model.refreshDisks() }
                    .keyboardShortcut("r", modifiers: [.command, .shift])
            }
        }
    }
}

struct DetectedDisk: Identifiable, Equatable {
    let id: String
    let bsd: String
    let name: String
    let mountPoint: String
    let rdisk: String
    let rawOk: Bool

    var title: String {
        if !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return name
        }
        if mountPoint.hasPrefix("/Volumes/") {
            let base = URL(fileURLWithPath: mountPoint).lastPathComponent
            if !base.isEmpty { return base }
        }
        return "NTFS USB (\(bsd))"
    }

    var subtitle: String {
        var parts = [bsd]
        if !mountPoint.isEmpty { parts.append(mountPoint) }
        return parts.joined(separator: " · ")
    }
}

struct SafetyInfo {
    var verified: Bool = false
    var dirty: Bool = false
    var hibernated: Bool = false
    var bitlocker: Bool = false
    var efsPresent: Bool = false
    var writableSafe: Bool = false
    var probeError: String?

    var summaryLines: [String] {
        var lines: [String] = []
        if let probeError { lines.append(probeError) }
        if !verified { lines.append("NTFS boot sector not verified") }
        if dirty { lines.append("$Volume dirty flag set") }
        if hibernated { lines.append("hiberfil.sys present (Windows fast startup)") }
        if bitlocker { lines.append("BitLocker detected") }
        if efsPresent { lines.append("EFS-encrypted files present") }
        if lines.isEmpty { lines.append("No safety warnings") }
        return lines
    }
}

enum MountBadge: String {
    case none = "Not mounted"
    case readOnly = "Read-Only"
    case readWrite = "Read-Write"
    case blocked = "Write Blocked"
    case finderMount = "Finder (read-only)"

    var color: Color {
        switch self {
        case .none: return .secondary
        case .readOnly: return .blue
        case .readWrite: return .green
        case .blocked: return .orange
        case .finderMount: return .teal
        }
    }
}

final class VolumeModel: ObservableObject {
    @Published var path: String = ""
    @Published var volumeTitle: String = ""
    @Published var cwd: String = "/"
    @Published var wantWrite: Bool = false
    @Published var engineWritable: Bool = false
    @Published var badge: MountBadge = .none
    @Published var entries: [DirRow] = []
    @Published var status: String = "Select an NTFS USB drive or open a disk image"
    @Published var log: String = ""
    @Published var safety = SafetyInfo()
    @Published var scanning = false
    @Published var busy: Bool = false
    @Published var busyMessage: String = ""
    @Published var busyCancellable: Bool = false
    @Published var mutating: Bool = false
    @Published var deviceWriteConfirmed = false
    @Published var disks: [DetectedDisk] = []
    @Published var hostBrowseRoot: URL?
    @Published var currentDisk: DetectedDisk?
    @Published var selectedName: String?
    @Published var editorText = ""
    @Published var editorPath = ""
    @Published var showEditor = false
    @Published var showNameSheet = false
    @Published var nameSheetTitle = "Name"
    @Published var nameSheetValue = ""
    @Published var nameSheetMode = NameMode.newFile
    @Published var showDeleteConfirm = false
    @Published var showEnableWrite = false
    @Published var actionError = ""

    enum NameMode {
        case newFile, newFolder, rename
    }

    private var handle: OpaquePointer?
    private var scopedURL: URL?
    private var elevateCancelled = false
    private var lastElevateError = ""
    private var workGen = UUID()

    var isMounted: Bool { handle != nil || hostBrowseRoot != nil }
    var usingHostBrowse: Bool { hostBrowseRoot != nil }
    var canGoUp: Bool { isMounted && cwd != "/" }

    deinit { closeEngine(remountFinder: true) }

    func isBlockDevicePath(_ path: String) -> Bool {
        var st = stat()
        if stat(path, &st) == 0 {
            let mode = st.st_mode
            return (mode & S_IFMT) == S_IFBLK || (mode & S_IFMT) == S_IFCHR
        }
        return path.hasPrefix("/dev/")
    }

    func appendLog(_ line: String) {
        let ts = ISO8601DateFormatter().string(from: Date())
        let entry = "[\(ts)] \(line)\n"
        if Thread.isMainThread {
            log += entry
        } else {
            DispatchQueue.main.async { self.log += entry }
        }
    }

    func joinPath(_ dir: String, _ name: String) -> String {
        if dir == "/" { return "/\(name)" }
        return "\(dir)/\(name)"
    }

    func parentPath(_ dir: String) -> String {
        if dir == "/" { return "/" }
        let trimmed = dir.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        guard let slash = trimmed.lastIndex(of: "/") else { return "/" }
        return "/" + trimmed[..<slash]
    }

    func refreshDisks() {
        if scanning { return }
        scanning = true
        status = "Scanning for NTFS disks…"
        appendLog("scanning NTFS disks")
        DispatchQueue.global(qos: .userInitiated).async {
            var buf = [CChar](repeating: 0, count: 16 * 1024)
            let n = myntfs_list_disks(&buf, buf.count)
            let err = n < 0 ? String(cString: myntfs_last_error()) : ""
            let text = n < 0 ? "" : String(cString: buf)
            var found: [DetectedDisk] = []
            if n >= 0 {
                for line in text.split(separator: "\n") {
                    let p = line.split(separator: "|", omittingEmptySubsequences: false).map(String.init)
                    guard p.count >= 5 else { continue }
                    found.append(DetectedDisk(
                        id: p[0],
                        bsd: p[0],
                        name: p[1],
                        mountPoint: p[2],
                        rdisk: p[3],
                        rawOk: p[4] == "1"
                    ))
                }
            }
            DispatchQueue.main.async {
                self.scanning = false
                if n < 0 {
                    self.disks = []
                    self.status = "Disk scan failed: \(err)"
                    self.appendLog("disk scan failed: \(err)")
                    return
                }
                self.disks = found
                self.appendLog("found \(found.count) NTFS disk(s): \(found.map(\.title).joined(separator: ", "))")
                if found.isEmpty {
                    self.status = "No NTFS disks found. Plug in a USB drive or open a .img file."
                } else if !self.isMounted {
                    self.status = found.count == 1
                        ? "Found \(found[0].title). Click it to explore."
                        : "Found \(found.count) NTFS disks. Click one to explore."
                }
            }
        }
    }

    func probe(path: String) -> SafetyInfo {
        var report = MyNtfsSafetyReport()
        var err = [CChar](repeating: 0, count: 512)
        let rc = myntfs_probe(path, &report, &err, err.count)
        if rc != 0 {
            return SafetyInfo(probeError: String(cString: err))
        }
        return SafetyInfo(
            verified: report.verified != 0,
            dirty: report.dirty != 0,
            hibernated: report.hibernated != 0,
            bitlocker: report.bitlocker != 0,
            efsPresent: report.efs_present != 0,
            writableSafe: report.writable_safe != 0
        )
    }

    func openDisk(_ disk: DetectedDisk) {
        if busy { return }
        busy = true
        busyMessage = "Opening \(disk.title)…"
        appendLog("opening \(disk.title) (\(disk.bsd))")
        DispatchQueue.global(qos: .userInitiated).async {
            let folder = self.ensureFinderMounted(disk)
            var err = [CChar](repeating: 0, count: 512)
            var engine: OpaquePointer?
            if folder == nil {
                engine = myntfs_mount_ex(disk.rdisk, 0, 0, &err, err.count)
            }
            let engineErr = engine == nil && folder == nil ? String(cString: err) : ""
            DispatchQueue.main.async {
                self.busy = false
                self.busyMessage = ""
                self.closeEngine(remountFinder: true)
                self.wantWrite = false
                self.deviceWriteConfirmed = false
                self.cwd = "/"
                self.volumeTitle = disk.title
                self.currentDisk = disk
                if let folder {
                    self.hostBrowseRoot = folder
                    self.path = folder.path
                    self.badge = .finderMount
                    self.status = "Exploring \(disk.title) — folders, files, and (after Enable writes) create/edit/delete."
                    self.refresh()
                    self.appendLog("explorer on \(folder.path)")
                    return
                }
                if let engine {
                    self.handle = engine
                    self.path = disk.rdisk
                    self.safety = self.safetyFromVolume(engine)
                    self.engineWritable = myntfs_is_writable(engine) != 0
                    self.updateBadge(requestWrite: false)
                    self.refresh()
                    self.appendLog("engine mounted \(disk.rdisk)")
                    return
                }
                self.badge = .none
                self.status = "Cannot open \(disk.title). If macOS asked for Removable Volumes access, allow it and try again."
                self.actionError = engineErr.isEmpty
                    ? "Could not open \(disk.title) at \(disk.mountPoint.isEmpty ? disk.rdisk : disk.mountPoint)."
                    : engineErr
                self.appendLog("open failed: \(self.actionError)")
            }
        }
    }

    func openImagePanel() {
        let panel = NSOpenPanel()
        panel.title = "Open NTFS disk image"
        panel.message = "Choose an .img / .dmg / raw NTFS image file."
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.item]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        wantWrite = false
        deviceWriteConfirmed = false
        open(url: url, requestWrite: false, allowDeviceWrite: false)
    }

    func ensureFinderMounted(_ disk: DetectedDisk) -> URL? {
        func volumeURL(_ path: String) -> URL? {
            let p = path.trimmingCharacters(in: .whitespacesAndNewlines)
            guard p.hasPrefix("/Volumes/"), p != "/Volumes", p != "/Volumes/" else { return nil }
            return URL(fileURLWithPath: p, isDirectory: true)
        }
        if isVolumeMounted(disk.bsd), let url = volumeURL(disk.mountPoint) {
            return url
        }
        _ = runDiskutil(["mount", disk.bsd])
        Thread.sleep(forTimeInterval: 0.5)
        if let fromInfo = mountPointFromDiskutil(disk.bsd), let url = volumeURL(fromInfo) {
            return url
        }
        if let url = volumeURL("/Volumes/\(disk.title)") {
            return url
        }
        return usableMountURL(disk.mountPoint)
    }

    func usableMountURL(_ path: String) -> URL? {
        let p = path.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !p.isEmpty, p != "Not applicable", p != "/", p.hasPrefix("/Volumes/") else {
            return nil
        }
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: p, isDirectory: &isDir), isDir.boolValue else {
            return nil
        }
        return URL(fileURLWithPath: p, isDirectory: true)
    }

    func mountPointFromDiskutil(_ bsd: String) -> String? {
        let out = runDiskutil(["info", bsd])
        for line in out.split(separator: "\n") {
            let t = line.trimmingCharacters(in: .whitespaces)
            if t.hasPrefix("Mount Point:") {
                return String(t.dropFirst("Mount Point:".count)).trimmingCharacters(in: .whitespaces)
            }
        }
        return nil
    }

    func runDiskutil(_ args: [String]) -> String {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/sbin/diskutil")
        proc.arguments = args
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = pipe
        do {
            try proc.run()
            proc.waitUntilExit()
        } catch {
            appendLog("diskutil \(args.joined(separator: " ")) failed: \(error.localizedDescription)")
            return ""
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        return String(decoding: data, as: UTF8.self)
    }

    func open(url: URL, requestWrite: Bool, allowDeviceWrite: Bool) {
        close()
        _ = url.startAccessingSecurityScopedResource()
        scopedURL = url
        path = url.path
        cwd = "/"
        volumeTitle = url.lastPathComponent
        safety = probe(path: path)
        appendLog("probe \(path) verified=\(safety.verified) writable_safe=\(safety.writableSafe)")

        var err = [CChar](repeating: 0, count: 512)
        let allowDev: Int32 = (allowDeviceWrite && requestWrite) ? 1 : 0
        let wr: Int32 = requestWrite ? 1 : 0
        handle = myntfs_mount_ex(path, wr, allowDev, &err, err.count)
        if handle == nil {
            badge = .none
            engineWritable = false
            status = String(cString: err)
            appendLog("mount failed: \(status)")
            return
        }
        engineWritable = myntfs_is_writable(handle) != 0
        updateBadge(requestWrite: requestWrite)
        refresh()
        appendLog("mounted \(path) badge=\(badge.rawValue)")
    }

    func safetyFromVolume(_ handle: OpaquePointer) -> SafetyInfo {
        var report = MyNtfsSafetyReport()
        guard myntfs_volume_safety(handle, &report) == 0 else {
            return SafetyInfo()
        }
        return SafetyInfo(
            verified: report.verified != 0,
            dirty: report.dirty != 0,
            hibernated: report.hibernated != 0,
            bitlocker: report.bitlocker != 0,
            efsPresent: report.efs_present != 0,
            writableSafe: report.writable_safe != 0
        )
    }

    func close() {
        closeEngine(remountFinder: currentDisk != nil)
    }

    func closeVolume() {
        closeEngine(remountFinder: true)
        status = "Volume closed. Finder can remount the USB read-only."
    }

    func closeEngine(remountFinder: Bool) {
        let disk = currentDisk
        if let h = handle {
            myntfs_umount(h)
            handle = nil
        }
        myntfs_da_release()
        if let u = scopedURL {
            u.stopAccessingSecurityScopedResource()
            scopedURL = nil
        }
        hostBrowseRoot = nil
        currentDisk = nil
        selectedName = nil
        entries = []
        engineWritable = false
        wantWrite = false
        deviceWriteConfirmed = false
        badge = .none
        cwd = "/"
        volumeTitle = ""
        if remountFinder, let disk, let folder = ensureFinderMounted(disk) {
            appendLog("Finder remounted \(folder.path) read-only")
        }
    }

    private func updateBadge(requestWrite: Bool) {
        if handle == nil && hostBrowseRoot == nil {
            badge = .none
        } else if engineWritable {
            badge = .readWrite
            status = "Exploring \(volumeTitle) read-write"
        } else if requestWrite {
            badge = .blocked
            status = "Write requested but blocked by safety gates"
        } else if handle != nil {
            badge = .readOnly
            status = "Exploring \(volumeTitle) read-only"
        }
    }

    func goUp() {
        guard canGoUp else { return }
        cwd = parentPath(cwd)
        refresh()
    }

    func goHome() {
        cwd = "/"
        refresh()
    }

    func activate(_ row: DirRow) {
        if row.isDir {
            cwd = joinPath(cwd, row.name)
            refresh()
            return
        }
        openFile(row)
    }

    func currentNtfsPath(_ name: String) -> String {
        joinPath(cwd, name)
    }

    func currentHostURL(_ name: String? = nil) -> URL? {
        guard var url = hostBrowseRoot else { return nil }
        let rel = cwd.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        if !rel.isEmpty {
            for part in rel.split(separator: "/") {
                url.appendPathComponent(String(part), isDirectory: true)
            }
        }
        if let name {
            url.appendPathComponent(name)
        }
        return url
    }

    func refresh() {
        if hostBrowseRoot != nil {
            refreshHost()
            return
        }
        guard let h = handle else { return }
        var names = [CChar](repeating: 0, count: 256 * 1024)
        var isDir = [UInt8](repeating: 0, count: 2048)
        var sizes = [UInt64](repeating: 0, count: 2048)
        let n = myntfs_listdir(h, cwd, &names, names.count, &isDir, &sizes, 2048)
        guard n >= 0 else {
            status = String(cString: myntfs_last_error())
            appendLog("listdir \(cwd) failed: \(status)")
            return
        }
        var rows: [DirRow] = []
        var off = 0
        for i in 0..<Int(n) {
            let cstr = names.withUnsafeBufferPointer { buf in
                buf.baseAddress!.advanced(by: off)
            }
            let name = String(cString: cstr)
            off += name.utf8.count + 1
            rows.append(DirRow(name: name, isDir: isDir[i] != 0, size: sizes[i]))
        }
        entries = rows.sorted {
            if $0.isDir != $1.isDir { return $0.isDir && !$1.isDir }
            return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
        status = "\(volumeTitle)\(cwd)  —  \(entries.count) items"
    }

    func refreshHost() {
        guard let folder = currentHostURL() else { return }
        do {
            let urls = try FileManager.default.contentsOfDirectory(
                at: folder,
                includingPropertiesForKeys: [.isDirectoryKey, .fileSizeKey],
                options: []
            )
            entries = urls.compactMap { url -> DirRow? in
                let name = url.lastPathComponent
                if name == ".DS_Store" || name.hasPrefix("._") { return nil }
                let vals = try? url.resourceValues(forKeys: [.isDirectoryKey, .fileSizeKey])
                return DirRow(
                    name: name,
                    isDir: vals?.isDirectory == true,
                    size: UInt64(vals?.fileSize ?? 0)
                )
            }
            .sorted {
                if $0.isDir != $1.isDir { return $0.isDir && !$1.isDir }
                return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
            }
            status = "\(volumeTitle)\(cwd)  —  \(entries.count) items"
        } catch {
            status = "Could not list \(folder.path): \(error.localizedDescription). Allow MyNTFS for Removable Volumes in System Settings if macOS asked."
            appendLog(status)
        }
    }

    func openFile(_ row: DirRow) {
        if let url = currentHostURL(row.name) {
            NSWorkspace.shared.open(url)
            appendLog("opened \(url.path)")
            return
        }
        guard handle != nil else { return }
        busy = true
        defer { busy = false }
        let tmp = FileManager.default.temporaryDirectory
            .appendingPathComponent("MyNTFS-open", isDirectory: true)
        try? FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        let dest = tmp.appendingPathComponent(row.name)
        copyOut(entry: row, to: dest)
        NSWorkspace.shared.open(dest)
    }

    func revealInFinder(_ row: DirRow) {
        if let url = currentHostURL(row.name) {
            NSWorkspace.shared.activateFileViewerSelecting([url])
        }
    }

    func copyOut(entry: DirRow, to dest: URL) {
        busy = true
        defer { busy = false }
        if let src = currentHostURL(entry.name) {
            do {
                if FileManager.default.fileExists(atPath: dest.path) {
                    try FileManager.default.removeItem(at: dest)
                }
                try FileManager.default.copyItem(at: src, to: dest)
                appendLog("copied \(entry.name) → \(dest.path)")
            } catch {
                appendLog("copy failed \(entry.name): \(error.localizedDescription)")
            }
            return
        }
        guard let h = handle, !entry.isDir else { return }
        let n = myntfs_copy_out(h, currentNtfsPath(entry.name), dest.path)
        if n < 0 {
            appendLog("copy failed \(entry.name): \(String(cString: myntfs_last_error()))")
        } else {
            appendLog("copied \(entry.name) (\(n) bytes) → \(dest.path)")
        }
    }

    var selectedRow: DirRow? {
        entries.first { $0.name == selectedName }
    }

    var canMutate: Bool { handle != nil && engineWritable }

    func lastErr() -> String { String(cString: myntfs_last_error()) }

    func beginNewFile() {
        guard ensureWritable() else { return }
        nameSheetMode = .newFile
        nameSheetTitle = "New file"
        nameSheetValue = "untitled.txt"
        showNameSheet = true
    }

    func beginNewFolder() {
        guard ensureWritable() else { return }
        nameSheetMode = .newFolder
        nameSheetTitle = "New folder"
        nameSheetValue = "New Folder"
        showNameSheet = true
    }

    func beginRename() {
        guard ensureWritable(), let row = selectedRow else { return }
        nameSheetMode = .rename
        nameSheetTitle = "Rename"
        nameSheetValue = row.name
        showNameSheet = true
    }

    func submitNameSheet() {
        let name = nameSheetValue.trimmingCharacters(in: .whitespacesAndNewlines)
        showNameSheet = false
        guard !name.isEmpty else { return }
        switch nameSheetMode {
        case .newFile: createFile(named: name)
        case .newFolder: createFolder(named: name)
        case .rename: renameSelected(to: name)
        }
    }

    func ensureWritable() -> Bool {
        if canMutate { return true }
        showEnableWrite = true
        return false
    }

    func createFile(named name: String) {
        runEngineWork(message: "Creating file…") { h in
            myntfs_create(h, self.cwd, name) != 0 ? self.lastErr() : nil
        } onSuccess: {
            self.appendLog("created file \(name)")
            self.selectedName = name
        }
    }

    func createFolder(named name: String) {
        runEngineWork(message: "Creating folder…") { h in
            myntfs_mkdir(h, self.cwd, name) != 0 ? self.lastErr() : nil
        } onSuccess: {
            self.appendLog("created folder \(name)")
            self.selectedName = name
        }
    }

    func renameSelected(to newName: String) {
        guard let row = selectedRow else { return }
        runEngineWork(message: "Renaming…") { h in
            myntfs_rename(h, self.currentNtfsPath(row.name), newName) != 0 ? self.lastErr() : nil
        } onSuccess: {
            self.appendLog("renamed \(row.name) → \(newName)")
            self.selectedName = newName
        }
    }

    func deleteSelected() {
        guard ensureWritable(), selectedRow != nil else { return }
        showDeleteConfirm = true
    }

    func confirmDelete() {
        showDeleteConfirm = false
        guard let row = selectedRow else { return }
        runEngineWork(message: "Deleting…") { h in
            let path = self.currentNtfsPath(row.name)
            let rc = row.isDir ? myntfs_rmdir(h, path) : myntfs_unlink(h, path)
            return rc != 0 ? self.lastErr() : nil
        } onSuccess: {
            self.appendLog("deleted \(row.name)")
            self.selectedName = nil
        }
    }

    func editSelected() {
        guard ensureWritable(), let row = selectedRow, !row.isDir, let h = handle else { return }
        let path = currentNtfsPath(row.name)
        var isDir: Int32 = 0
        let size = myntfs_stat_size(h, path, &isDir)
        if size < 0 {
            actionError = lastErr()
            return
        }
        if size > 2 * 1024 * 1024 {
            actionError = "File is larger than 2 MB — copy it out to edit."
            return
        }
        var buf = [UInt8](repeating: 0, count: Int(size) + 1)
        let n = buf.withUnsafeMutableBytes { raw -> Int64 in
            myntfs_read(h, path, 0, raw.baseAddress, Int(size))
        }
        if n < 0 {
            actionError = lastErr()
            return
        }
        let data = Data(buf.prefix(Int(n)))
        guard let text = String(data: data, encoding: .utf8) else {
            actionError = "This file is not UTF-8 text. Copy it to the Mac to edit."
            return
        }
        editorPath = path
        editorText = text
        showEditor = true
    }

    func saveEditor() {
        let data = Data(editorText.utf8)
        let dest = editorPath
        runEngineWork(message: "Saving…") { h in
            let n = data.withUnsafeBytes { raw -> Int64 in
                guard let p = raw.baseAddress else { return 0 }
                return myntfs_write_contents(h, dest, p, data.count)
            }
            return n < 0 ? self.lastErr() : nil
        } onSuccess: {
            self.appendLog("saved \(dest) (\(data.count) bytes)")
            self.showEditor = false
        }
    }

    func importFile(from url: URL) {
        guard ensureWritable() else { return }
        _ = url.startAccessingSecurityScopedResource()
        runEngineWork(message: "Importing…") { h in
            defer { url.stopAccessingSecurityScopedResource() }
            return myntfs_copy_in(h, url.path, self.cwd, url.lastPathComponent) < 0 ? self.lastErr() : nil
        } onSuccess: {
            self.appendLog("imported \(url.lastPathComponent)")
            self.selectedName = url.lastPathComponent
        }
    }

    func runEngineWork(message: String, _ work: @escaping (OpaquePointer) -> String?, onSuccess: (() -> Void)? = nil) {
        guard let h = handle else { return }
        guard !busy else { return }
        busy = true
        busyCancellable = false
        mutating = true
        busyMessage = message
        let gen = UUID()
        workGen = gen
        DispatchQueue.global(qos: .userInitiated).async {
            let err = work(h)
            DispatchQueue.main.async {
                guard self.workGen == gen else { return }
                self.busy = false
                self.busyCancellable = false
                self.mutating = false
                self.busyMessage = ""
                if let err {
                    self.actionError = err
                    self.appendLog(err)
                } else {
                    onSuccess?()
                    self.refresh()
                }
            }
        }
    }

    func enableWrites() {
        showEnableWrite = false
        if let url = scopedURL, !isBlockDevicePath(url.path) {
            enableWritesOnImage(url)
            return
        }
        guard let disk = currentDisk ?? disks.first(where: { $0.title == volumeTitle }) else {
            actionError = "No disk selected."
            return
        }
        guard !busy else { return }
        busy = true
        busyCancellable = true
        mutating = false
        busyMessage = "Taking exclusive access (Finder stay unmounted)…"
        elevateCancelled = false
        lastElevateError = ""
        let gen = UUID()
        workGen = gen
        appendLog("enable writes on \(disk.title) \(disk.rdisk)")
        let diskCopy = disk
        DispatchQueue.global(qos: .userInitiated).async {
            self.enableWritesOnUSB(diskCopy, gen: gen)
        }
    }

    func enableWritesOnImage(_ url: URL) {
        wantWrite = true
        deviceWriteConfirmed = false
        open(url: url, requestWrite: true, allowDeviceWrite: false)
        if handle == nil {
            actionError = status.isEmpty ? "Could not open the disk image for writing." : status
        } else if !engineWritable {
            actionError = safetyMessage()
        }
    }

    var enableWritesIsImage: Bool {
        if let url = scopedURL, !isBlockDevicePath(url.path) { return true }
        if currentDisk != nil { return false }
        return !path.isEmpty && !isBlockDevicePath(path)
    }

    func safetyMessage() -> String {
        let reasons = safety.summaryLines.filter { $0 != "No safety warnings" }
        if reasons.isEmpty {
            return "Mounted, but safety gates blocked write. Check dirty / BitLocker / hibernate."
        }
        return "Write blocked by safety gates: \(reasons.joined(separator: "; "))."
    }

    func enableWritesOnUSB(_ disk: DetectedDisk, gen: UUID) {
        func cancelled() -> Bool {
            elevateCancelled || workGen != gen
        }
        func fail(_ message: String, remount: Bool) {
            DispatchQueue.main.async {
                guard self.workGen == gen else { return }
                self.busy = false
                self.busyCancellable = false
                self.mutating = false
                self.busyMessage = ""
                myntfs_da_release()
                self.actionError = message
                self.appendLog(message)
                if remount, let folder = self.ensureFinderMounted(disk) {
                    self.hostBrowseRoot = folder
                    self.path = folder.path
                    self.badge = .finderMount
                    self.engineWritable = false
                    self.refresh()
                    self.appendLog("write failed; remounted Finder at \(folder.path)")
                }
            }
        }

        if cancelled() {
            return
        }

        var err = [CChar](repeating: 0, count: 512)
        let holdRc = myntfs_da_hold(disk.bsd, &err, err.count)
        if cancelled() {
            if holdRc == 0 { myntfs_da_release() }
            return
        }
        if holdRc != 0 {
            let detail = String(cString: err)
            fail(detail.isEmpty
                 ? "Could not take exclusive access to the USB volume."
                 : detail, remount: true)
            return
        }
        if myntfs_da_slice_mounted(disk.bsd) != 0 {
            fail("macOS still has this volume mounted (FSKit). MyNTFS could not take exclusive raw access. Try again, or unmount in Disk Utility first.", remount: true)
            return
        }
        if cancelled() {
            myntfs_da_release()
            return
        }

        DispatchQueue.main.async {
            guard self.workGen == gen else { return }
            self.busyMessage = "macOS is asking for permission to open the disk…"
        }
        let fd = openWritableRdisk(disk.rdisk)
        if cancelled() {
            if fd >= 0 { Darwin.close(fd) }
            myntfs_da_release()
            return
        }
        if fd < 0 {
            let mapped = lastElevateError.isEmpty
                ? "Could not open raw disk for writing (permission or helper failed)."
                : lastElevateError
            fail(mapped, remount: true)
            return
        }

        if myntfs_da_ensure_unmounted(&err, err.count) != 0 {
            Darwin.close(fd)
            if cancelled() { return }
            let detail = String(cString: err)
            fail(detail.isEmpty
                 ? "Volume remounted during authorization. Writes were not enabled."
                 : detail, remount: true)
            return
        }

        let writableFd = myntfs_fd_writable(fd) != 0
        let preadOk = myntfs_fd_pread_ok(fd) != 0
        let fl = myntfs_fd_getfl(fd)
        appendLog("writable fd=\(fd) getfl=\(fl) pread=\(preadOk) da_hold=\(myntfs_da_holding())")
        if cancelled() {
            Darwin.close(fd)
            myntfs_da_release()
            return
        }
        if !writableFd || !preadOk {
            Darwin.close(fd)
            fail("Opened the disk, but the file descriptor is not writable. Refusing a read-only fallback.", remount: true)
            return
        }

        DispatchQueue.main.async {
            guard self.workGen == gen else { return }
            self.busyMessage = "Mounting NTFS read-write…"
        }
        if cancelled() {
            Darwin.close(fd)
            myntfs_da_release()
            return
        }
        let mounted = myntfs_mount_fd(fd, disk.rdisk, 1, 1, &err, err.count)
        Darwin.close(fd)
        if cancelled() {
            if let mounted { myntfs_umount(mounted) }
            return
        }
        guard let mounted else {
            let msg = String(cString: err)
            if msg.lowercased().contains("write denied") || msg.lowercased().contains("safety") {
                fail(safetyMapped(msg), remount: true)
            } else {
                fail("Could not enable writes (\(msg)). Exploring read-only instead.", remount: true)
            }
            return
        }

        let writable = myntfs_is_writable(mounted) != 0
        let safetyInfo = safetyFromVolume(mounted)
        DispatchQueue.main.async {
            if self.workGen != gen || self.elevateCancelled {
                myntfs_umount(mounted)
                return
            }
            self.busy = false
            self.busyCancellable = false
            self.mutating = false
            self.busyMessage = ""
            if let old = self.handle {
                myntfs_umount(old)
            }
            self.hostBrowseRoot = nil
            self.handle = mounted
            self.currentDisk = disk
            self.volumeTitle = disk.title
            self.cwd = "/"
            self.path = disk.rdisk
            self.engineWritable = writable
            self.safety = safetyInfo
            if !writable {
                self.actionError = self.safetyMessage()
                self.badge = .blocked
            } else {
                self.badge = .readWrite
                self.wantWrite = true
                self.deviceWriteConfirmed = true
                self.status = "Exploring \(disk.title) read-write"
            }
            self.refresh()
            self.appendLog("write enabled=\(writable) via owned fd on \(disk.rdisk)")
        }
    }

    func safetyMapped(_ engine: String) -> String {
        if engine.lowercased().contains("dirty") {
            return "Write blocked: the volume dirty flag is set. Check the disk in Windows or Disk Utility first."
        }
        if engine.lowercased().contains("hiber") {
            return "Write blocked: Windows hibernation / fast startup is present (hiberfil.sys)."
        }
        if engine.lowercased().contains("bitlocker") {
            return "Write blocked: BitLocker encryption was detected."
        }
        return "Write blocked by safety gates (\(engine))."
    }

    func allowedRdisk(_ path: String) -> Bool {
        path.range(of: #"^/dev/rdisk[0-9]+s[0-9]+$"#, options: .regularExpression) != nil
    }

    func isVolumeMounted(_ bsd: String) -> Bool {
        myntfs_da_slice_mounted(bsd) != 0
    }

    func run(_ exe: String, args: [String]) -> String {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: exe)
        proc.arguments = args
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = pipe
        do {
            try proc.run()
            proc.waitUntilExit()
        } catch {
            return ""
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        return String(decoding: data, as: UTF8.self)
    }

    func mapHelperError(_ raw: String) -> String {
        let t = raw.lowercased()
        if t.contains("still mounted") || t.contains("fskit") {
            return "macOS still has this volume mounted (FSKit). MyNTFS could not take exclusive raw access. Try again, or unmount in Disk Utility first."
        }
        if t.contains("removable") || t.contains("full disk access") || t.contains("authopen") {
            return "macOS blocked raw disk access. In System Settings → Privacy & Security, allow MyNTFS for Removable Volumes (or Full Disk Access), then try Enable writes again."
        }
        if t.contains("authorization cancelled") {
            return "Authorization cancelled. Writes were not enabled."
        }
        if t.contains("paragon") || t.contains("another driver") {
            return "Another driver still holds this volume. Unmount it in Disk Utility, then try again."
        }
        if t.contains("internal") || t.contains("boot") {
            return "Refusing to unmount an internal or boot disk."
        }
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return "Could not open raw disk for writing (permission or helper failed)."
        }
        return "Could not open raw disk for writing. \(trimmed)"
    }

    /// Open O_RDWR, keeping the fd. Never falls back to O_RDONLY.
    func openWritableRdisk(_ rdisk: String) -> Int32 {
        lastElevateError = ""
        guard allowedRdisk(rdisk) else {
            appendLog("rdisk allowlist rejected \(rdisk) (slice only)")
            lastElevateError = "Refusing to open a whole disk or non-rdisk path."
            return -1
        }
        if elevateCancelled {
            lastElevateError = "Enable writes cancelled."
            return -1
        }

        var err = [CChar](repeating: 0, count: 512)
        var received: Int32 = -1
        if Thread.isMainThread {
            received = myntfs_authopen_rdisk(rdisk, &err, err.count)
        } else {
            let path = rdisk
            let pair: (Int32, [CChar]) = DispatchQueue.main.sync {
                var e = [CChar](repeating: 0, count: 512)
                let fd = myntfs_authopen_rdisk(path, &e, e.count)
                return (fd, e)
            }
            received = pair.0
            err = pair.1
        }

        let helperErr = String(cString: err)
        lastElevateError = mapHelperError(helperErr)
        let writable = received >= 0 && myntfs_fd_writable(received) != 0
        let preadOk = received >= 0 && myntfs_fd_pread_ok(received) != 0
        let fl = received >= 0 ? myntfs_fd_getfl(received) : -1
        appendLog("authopen \(rdisk) fd=\(received) writable=\(writable) pread=\(preadOk) getfl=\(fl)")
        if received >= 0 && (!writable || !preadOk || elevateCancelled) {
            Darwin.close(received)
            if lastElevateError.isEmpty {
                lastElevateError = elevateCancelled
                    ? "Enable writes cancelled."
                    : "Opened the disk, but the file descriptor is not writable."
            }
            return -1
        }
        if received < 0 && lastElevateError.isEmpty {
            lastElevateError = helperErr.isEmpty
                ? "Could not open raw disk for writing (permission or helper failed)."
                : mapHelperError(helperErr)
        }
        return received
    }

    func cancelBusyWork() {
        guard busyCancellable, !mutating else { return }
        elevateCancelled = true
        workGen = UUID()
        busy = false
        busyCancellable = false
        busyMessage = ""
        myntfs_da_release()
        appendLog("enable-writes cancelled; releasing exclusive access")
        if let disk = currentDisk ?? disks.first(where: { $0.title == volumeTitle }) {
            if let folder = ensureFinderMounted(disk) {
                hostBrowseRoot = folder
                path = folder.path
                badge = .finderMount
                engineWritable = false
                refresh()
            }
        }
    }
}

struct DirRow: Identifiable, Hashable {
    var id: String { name }
    let name: String
    let isDir: Bool
    let size: UInt64
}

struct ContentView: View {
    @EnvironmentObject var model: VolumeModel
    @State private var showLogExport = false
    @State private var copyTarget: DirRow?
    @State private var showCopySave = false
    @State private var showImport = false

    var body: some View {
        NavigationSplitView {
            List {
                Section("Drives") {
                    if model.scanning {
                        HStack {
                            ProgressView().controlSize(.small)
                            Text("Scanning…")
                        }
                        .foregroundStyle(.secondary)
                    } else if model.disks.isEmpty {
                        Text("No NTFS disks")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(model.disks) { disk in
                        Button {
                            model.openDisk(disk)
                        } label: {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(disk.title)
                                Text(disk.subtitle)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .buttonStyle(.plain)
                    }
                    Button("Scan disks") { model.refreshDisks() }
                        .disabled(model.scanning || model.busy)
                    Button("Open image…") { model.openImagePanel() }
                        .disabled(model.busy)
                }
            }
            .navigationTitle("MyNTFS")
            .listStyle(.sidebar)
        } detail: {
            VStack(spacing: 0) {
                explorerBar
                Divider()
                if model.isMounted {
                    fileList
                } else {
                    emptyState
                }
            }
        }
        .onAppear { model.refreshDisks() }
        .overlay {
            if model.busy {
                ZStack {
                    Color.black.opacity(0.25).ignoresSafeArea()
                    VStack(spacing: 12) {
                        ProgressView()
                        Text(model.busyMessage.isEmpty ? "Working…" : model.busyMessage)
                            .multilineTextAlignment(.center)
                        if model.mutating {
                            Text("This write is in progress and cannot be cancelled.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        } else if model.busyCancellable {
                            Button("Cancel") { model.cancelBusyWork() }
                        }
                    }
                    .padding(24)
                    .frame(minWidth: 280)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
                }
            }
        }
        .fileExporter(isPresented: $showLogExport, document: LogDocument(text: model.log), contentType: .plainText, defaultFilename: "myntfs-log") { _ in }
        .fileExporter(isPresented: $showCopySave, document: EmptyDocument(), contentType: .data, defaultFilename: copyTarget?.name ?? "file") { result in
            if case .success(let url) = result, let row = copyTarget {
                model.copyOut(entry: row, to: url)
            }
        }
        .fileImporter(
            isPresented: $showImport,
            allowedContentTypes: [.item],
            allowsMultipleSelection: false
        ) { result in
            if case .success(let urls) = result, let url = urls.first {
                model.importFile(from: url)
            }
        }
        .alert("Name", isPresented: $model.showNameSheet) {
            TextField(model.nameSheetTitle, text: $model.nameSheetValue)
            Button("Cancel", role: .cancel) {}
            Button("OK") { model.submitNameSheet() }
        } message: {
            Text(model.nameSheetTitle)
        }
        .alert("Delete “\(model.selectedRow?.name ?? "")”?", isPresented: $model.showDeleteConfirm) {
            Button("Cancel", role: .cancel) {}
            Button("Delete", role: .destructive) { model.confirmDelete() }
        } message: {
            Text("This cannot be undone.")
        }
        .alert("Enable writes on \(model.volumeTitle.isEmpty ? "this disk" : model.volumeTitle)?", isPresented: $model.showEnableWrite) {
            Button("Cancel", role: .cancel) {}
            Button("Enable writes", role: .destructive) { model.enableWrites() }
        } message: {
            Text(enableWriteMessage)
        }
        .alert("MyNTFS", isPresented: Binding(
            get: { !model.actionError.isEmpty },
            set: { if !$0 { model.actionError = "" } }
        )) {
            Button("OK", role: .cancel) { model.actionError = "" }
        } message: {
            Text(model.actionError)
        }
        .sheet(isPresented: $model.showEditor) {
            VStack(alignment: .leading, spacing: 8) {
                Text("Edit \(model.editorPath)").font(.headline)
                TextEditor(text: $model.editorText)
                    .font(.system(.body, design: .monospaced))
                    .border(Color.secondary.opacity(0.3))
                HStack {
                    Button("Cancel") { model.showEditor = false }
                    Spacer()
                    Button("Save") { model.saveEditor() }
                        .keyboardShortcut(.defaultAction)
                }
            }
            .padding()
            .frame(minWidth: 520, minHeight: 360)
        }
    }

    private var enableWriteMessage: String {
        var lines: [String]
        if model.enableWritesIsImage {
            lines = [
                "This disk image will be opened read-write in MyNTFS.",
                "macOS will not ask for your password."
            ]
        } else {
            lines = [
                "Finder will force-unmount this USB volume so MyNTFS can keep exclusive access.",
                "macOS will ask for your password to open the raw disk.",
                "Do not do this if you cannot replace the files on the drive."
            ]
        }
        let safety = model.safety.summaryLines.filter { $0 != "No safety warnings" }
        if !safety.isEmpty {
            lines.append("Safety: " + safety.joined(separator: "; ") + ".")
        }
        return lines.joined(separator: " ")
    }

    private var explorerBar: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Button { model.goUp() } label: {
                    Image(systemName: "chevron.left")
                }
                .disabled(!model.canGoUp)
                .help("Back")

                Button { model.goHome() } label: {
                    Image(systemName: "house")
                }
                .disabled(!model.isMounted)
                .help("Volume root")

                Text(model.isMounted ? "\(model.volumeTitle)\(model.cwd)" : "No volume")
                    .font(.system(.body, design: .monospaced))
                    .lineLimit(1)
                    .truncationMode(.head)

                Spacer()

                Text(model.badge.rawValue)
                    .font(.caption.bold())
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(model.badge.color.opacity(0.2))
                    .foregroundStyle(model.badge.color)
                    .clipShape(Capsule())

                Button("Refresh") { model.refresh() }
                    .disabled(!model.isMounted || model.busy)
                Button("Export log…") { showLogExport = true }
                Button("Close volume") { model.closeVolume() }
                    .disabled(!model.isMounted || model.busy)

                if !model.canMutate && model.isMounted {
                    Button("Enable writes…") { model.showEnableWrite = true }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.busy)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)

            HStack(spacing: 8) {
                Button("New Folder") { model.beginNewFolder() }
                    .disabled(!model.canMutate || model.busy)
                    .help(model.canMutate ? "Create a folder" : "Enable writes to create folders")
                Button("New File") { model.beginNewFile() }
                    .disabled(!model.canMutate || model.busy)
                    .help(model.canMutate ? "Create a file" : "Enable writes to create files")
                Button("Rename") { model.beginRename() }
                    .disabled(!model.canMutate || model.selectedRow == nil || model.busy)
                Button("Edit") { model.editSelected() }
                    .disabled(!model.canMutate || model.selectedRow?.isDir != false || model.busy)
                Button("Delete", role: .destructive) { model.deleteSelected() }
                    .disabled(!model.canMutate || model.selectedRow == nil || model.busy)
                Button("Import…") { showImport = true }
                    .disabled(!model.canMutate || model.busy)
                    .help(model.canMutate ? "Copy a Mac file into the volume" : "Enable writes to import")
                Spacer()
            }
            .padding(.horizontal, 12)
            .padding(.bottom, 6)
        }
    }

    private var fileList: some View {
        VStack(spacing: 0) {
            if !model.canMutate && model.isMounted {
                HStack {
                    Text("Read-only until you enable writes. Apple’s NTFS mount cannot create or delete files.")
                        .font(.caption)
                    Spacer()
                    Button("Enable writes…") { model.showEnableWrite = true }
                }
                .padding(8)
                .background(Color.orange.opacity(0.15))
            }
            List(selection: $model.selectedName) {
                ForEach(model.entries) { row in
                    HStack {
                        Image(systemName: row.isDir ? "folder.fill" : "doc")
                            .foregroundStyle(row.isDir ? Color.accentColor : Color.secondary)
                            .frame(width: 20)
                        Text(row.name)
                            .lineLimit(1)
                        Spacer()
                        if !row.isDir {
                            Text(ByteCountFormatter.string(fromByteCount: Int64(row.size), countStyle: .file))
                                .foregroundStyle(.secondary)
                                .font(.caption)
                        }
                    }
                    .tag(row.name)
                    .contentShape(Rectangle())
                    .onTapGesture(count: 2) { model.activate(row) }
                    .contextMenu {
                        if row.isDir {
                            Button("Open") { model.activate(row) }
                            if model.canMutate {
                                Button("Rename…") {
                                    model.selectedName = row.name
                                    model.beginRename()
                                }
                                Button("Delete…", role: .destructive) {
                                    model.selectedName = row.name
                                    model.deleteSelected()
                                }
                            }
                        } else {
                            Button("Open") { model.activate(row) }
                            Button("Copy to Mac…") {
                                copyTarget = row
                                showCopySave = true
                            }
                            if model.usingHostBrowse {
                                Button("Reveal in Finder") { model.revealInFinder(row) }
                            }
                            if model.canMutate {
                                Button("Edit…") {
                                    model.selectedName = row.name
                                    model.editSelected()
                                }
                                Button("Rename…") {
                                    model.selectedName = row.name
                                    model.beginRename()
                                }
                                Button("Delete…", role: .destructive) {
                                    model.selectedName = row.name
                                    model.deleteSelected()
                                }
                            }
                        }
                        if model.canMutate {
                            Divider()
                            Button("New Folder…") { model.beginNewFolder() }
                            Button("New File…") { model.beginNewFile() }
                        } else if model.isMounted {
                            Divider()
                            Button("Enable writes…") { model.showEnableWrite = true }
                        }
                    }
                }
            }
            .listStyle(.inset)
                    .contextMenu {
                        Button("New Folder…") { model.beginNewFolder() }
                            .disabled(!model.canMutate)
                        Button("New File…") { model.beginNewFile() }
                            .disabled(!model.canMutate)
                        Button("Import from Mac…") { showImport = true }
                            .disabled(!model.canMutate)
                    }
            Text(model.status)
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
        }
    }

    private var emptyState: some View {
        VStack(spacing: 20) {
            Image(systemName: "externaldrive")
                .font(.system(size: 48))
                .foregroundStyle(.secondary)
            Text(model.status)
                .font(.headline)
                .multilineTextAlignment(.center)
                .padding(.horizontal)
            if model.scanning {
                ProgressView("Scanning for NTFS disks…")
            }
            if !model.disks.isEmpty {
                VStack(spacing: 10) {
                    ForEach(model.disks) { disk in
                        Button {
                            model.openDisk(disk)
                        } label: {
                            VStack(alignment: .leading, spacing: 4) {
                                Text("Explore \(disk.title)")
                                    .font(.headline)
                                Text(disk.subtitle)
                                    .font(.caption)
                            }
                            .frame(maxWidth: 360, alignment: .leading)
                            .padding(.vertical, 6)
                        }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.busy)
                    }
                }
            }
            HStack(spacing: 12) {
                Button("Scan NTFS disks") { model.refreshDisks() }
                    .disabled(model.scanning || model.busy)
                Button("Open disk image…") { model.openImagePanel() }
                    .disabled(model.busy)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
    }
}

struct LogDocument: FileDocument {
    static var readableContentTypes: [UTType] { [.plainText] }
    var text: String
    init(text: String) { self.text = text }
    init(configuration: ReadConfiguration) throws {
        text = String(decoding: configuration.file.regularFileContents ?? Data(), as: UTF8.self)
    }
    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
        FileWrapper(regularFileWithContents: Data(text.utf8))
    }
}

struct EmptyDocument: FileDocument {
    static var readableContentTypes: [UTType] { [.data] }
    init() {}
    init(configuration: ReadConfiguration) throws {}
    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
        FileWrapper(regularFileWithContents: Data())
    }
}
