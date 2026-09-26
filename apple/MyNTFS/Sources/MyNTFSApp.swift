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
                .onOpenURL { model.handleExternalURL($0) }
        }
        .defaultSize(width: 1180, height: 720)
        .windowToolbarStyle(.unified)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New Folder") { model.beginNewFolder() }
                    .keyboardShortcut("n", modifiers: [.command, .shift])
                    .disabled(!model.canMutate || model.busy)
                Button("New File") { model.beginNewFile() }
                    .keyboardShortcut("n")
                    .disabled(!model.canMutate || model.busy)
                Button("Import from Mac…") { model.presentImportPanel(foldersOnly: false) }
                    .keyboardShortcut("o")
                    .disabled(!model.canMutate || model.busy)
                Divider()
                Button("Refresh Disks") { model.refreshDisks() }
                    .keyboardShortcut("r", modifiers: [.command, .shift])
            }
            CommandGroup(after: .newItem) {
                Button("Get Info") { model.showGetInfo = true }
                    .keyboardShortcut("i")
                    .disabled(model.selectedRow == nil)
                Button("Duplicate") { model.duplicateSelected() }
                    .keyboardShortcut("d")
                    .disabled(!model.canMutate || model.selectedRow == nil || model.busy)
                Button("Copy Path") { model.copyNtfsPath() }
                    .keyboardShortcut("c", modifiers: [.command, .shift])
                    .disabled(model.selectedRow == nil)
            }
            CommandMenu("Go") {
                Button("Enclosing Folder") { model.goUp() }
                    .keyboardShortcut(.upArrow, modifiers: .command)
                    .disabled(!model.canGoUp)
                Button("Volume Root") { model.goHome() }
                    .keyboardShortcut(.upArrow, modifiers: [.command, .option])
                    .disabled(!model.isMounted)
            }
            CommandGroup(after: .help) {
                Button("Export Activity Log…") { NotificationCenter.default.post(name: .myntfsExportLog, object: nil) }
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
    let fileSystem: String
    let isNtfs: Bool

    var title: String {
        if !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return name
        }
        if mountPoint.hasPrefix("/Volumes/") {
            let base = URL(fileURLWithPath: mountPoint).lastPathComponent
            if !base.isEmpty { return base }
        }
        return "USB (\(bsd))"
    }

    var subtitle: String {
        var parts = [bsd]
        if !fileSystem.isEmpty { parts.append(fileSystem) }
        if !mountPoint.isEmpty { parts.append(mountPoint) }
        return parts.joined(separator: " · ")
    }

    var accessibilityTitle: String {
        if isNtfs {
            return "\(title), NTFS, \(subtitle)"
        }
        let fs = fileSystem.isEmpty ? "not NTFS" : fileSystem
        return "\(title), \(fs), \(subtitle)"
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
    case readWrite = "MyNTFS (read-write) · Finder hidden"
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
    @Published var lastDisk: DetectedDisk?
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
    @Published var searchText = ""
    @Published var hideSystemFiles = true
    @Published var showGetInfo = false
    @Published var volumeTotal: UInt64 = 0
    @Published var volumeFree: UInt64 = 0
    @Published var sortMode: SortMode = .name

    enum NameMode {
        case newFile, newFolder, rename
    }

    enum SortMode: String, CaseIterable, Identifiable {
        case name = "Name"
        case size = "Size"
        case kind = "Kind"
        var id: String { rawValue }
    }

    private var handle: OpaquePointer?
    private var scopedURL: URL?
    private var elevateCancelled = false
    private var lastElevateError = ""
    private var workGen = UUID()
    private var remountGen = UUID()

    var isMounted: Bool { handle != nil || hostBrowseRoot != nil }
    var usingHostBrowse: Bool { hostBrowseRoot != nil }
    var canGoUp: Bool { isMounted && cwd != "/" }

    deinit {
        if let h = handle {
            _ = myntfs_sync(h)
            myntfs_umount(h)
            handle = nil
        }
        myntfs_da_release()
        var spins = 0
        while myntfs_da_holding() != 0 && spins < 50 {
            Thread.sleep(forTimeInterval: 0.02)
            spins += 1
        }
        Self.cleanupStaleOpenTemps()
        if let disk = currentDisk ?? lastDisk {
            var pathbuf = [CChar](repeating: 0, count: 1024)
            var err = [CChar](repeating: 0, count: 512)
            _ = myntfs_da_mount_finder(disk.bsd, &pathbuf, pathbuf.count, &err, err.count)
        }
    }

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

    private static let openTempFolder = "MyNTFS-open"
    private static let openTempMaxAge: TimeInterval = 24 * 60 * 60

    private static let mtimeFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .medium
        f.timeStyle = .short
        return f
    }()

    static func cleanupStaleOpenTemps() {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(openTempFolder, isDirectory: true)
        let fm = FileManager.default
        guard let items = try? fm.contentsOfDirectory(
            at: root,
            includingPropertiesForKeys: [.contentModificationDateKey, .creationDateKey],
            options: []
        ) else { return }
        let cutoff = Date().addingTimeInterval(-openTempMaxAge)
        for url in items {
            let values = try? url.resourceValues(forKeys: [.contentModificationDateKey, .creationDateKey])
            let stamp = values?.contentModificationDate ?? values?.creationDate
            guard let stamp, stamp < cutoff else { continue }
            try? fm.removeItem(at: url)
        }
    }

    func formattedMtime(for row: DirRow) -> String {
        if usingHostBrowse {
            guard let url = currentHostURL(row.name),
                  let date = (try? url.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate
            else { return "—" }
            return Self.mtimeFormatter.string(from: date)
        }
        guard let h = handle else { return "—" }
        let sec = myntfs_stat_mtime(h, row.path)
        guard sec >= 0 else { return "—" }
        return Self.mtimeFormatter.string(from: Date(timeIntervalSince1970: TimeInterval(sec)))
    }

    func refreshDisks() {
        if scanning { return }
        scanning = true
        status = "Scanning for USB drives…"
        appendLog("scanning USB drives")
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
                    let isNtfs = p.count < 7 || p[6] == "1"
                    let fs = p.count >= 6 && !p[5].isEmpty ? p[5] : (isNtfs ? "NTFS" : "")
                    found.append(DetectedDisk(
                        id: p[0],
                        bsd: p[0],
                        name: p[1],
                        mountPoint: p[2],
                        rdisk: p[3],
                        rawOk: p[4] == "1",
                        fileSystem: fs,
                        isNtfs: isNtfs
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
                let ntfs = found.filter(\.isNtfs)
                self.appendLog("found \(found.count) USB volume(s): \(found.map { "\($0.title) (\($0.fileSystem))" }.joined(separator: ", "))")
                if found.isEmpty {
                    self.status = "No USB drives found. Plug in a drive or open a .img file."
                } else if !self.isMounted {
                    if ntfs.isEmpty {
                        self.status = "Found \(found.count) USB drive\(found.count == 1 ? "" : "s"), none NTFS. MyNTFS opens NTFS volumes."
                    } else if found.count == 1 {
                        self.status = "Found \(found[0].title). Click it to explore."
                    } else {
                        self.status = "Found \(found.count) USB drives (\(ntfs.count) NTFS). Click an NTFS volume to explore."
                    }
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
        if !disk.isNtfs {
            closeEngine(remountFinder: true, restoreBrowse: false)
            remountGen = UUID()
            wantWrite = false
            deviceWriteConfirmed = false
            cwd = "/"
            volumeTitle = disk.title
            currentDisk = disk
            lastDisk = disk
            hostBrowseRoot = nil
            handle = nil
            entries = []
            badge = .none
            status = "\(disk.title) is \(disk.fileSystem), not NTFS. MyNTFS only opens NTFS volumes."
            appendLog("skipped \(disk.title) (\(disk.bsd)): filesystem is \(disk.fileSystem)")
            return
        }
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
                self.closeEngine(remountFinder: true, restoreBrowse: false)
                self.remountGen = UUID()
                self.wantWrite = false
                self.deviceWriteConfirmed = false
                self.cwd = "/"
                self.volumeTitle = disk.title
                self.currentDisk = disk
                self.lastDisk = disk
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

    func revealInFinder(_ disk: DetectedDisk) {
        let path: String
        if !disk.mountPoint.isEmpty, FileManager.default.fileExists(atPath: disk.mountPoint) {
            path = disk.mountPoint
        } else {
            path = "/Volumes"
        }
        NSWorkspace.shared.open(URL(fileURLWithPath: path, isDirectory: true))
        appendLog("revealed \(disk.title) in Finder at \(path)")
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
        if isVolumeMounted(disk.bsd) {
            if let url = volumeURL(disk.mountPoint) { return url }
            if let fromInfo = mountPointFromDiskutil(disk.bsd), let url = volumeURL(fromInfo) {
                return url
            }
        }
        return mountFinderPath(disk)
    }

    /// DADiskMount + poll (≤10s). Must not be called while DA-holding.
    func mountFinderPath(_ disk: DetectedDisk) -> URL? {
        var pathbuf = [CChar](repeating: 0, count: 1024)
        var err = [CChar](repeating: 0, count: 512)
        let rc = myntfs_da_mount_finder(disk.bsd, &pathbuf, pathbuf.count, &err, err.count)
        if rc == 0 {
            let p = String(cString: pathbuf)
            if let url = usableMountURL(p) { return url }
        }
        let detail = String(cString: err)
        if !detail.isEmpty {
            appendLog("Finder remount failed: \(detail)")
        }
        if let fromInfo = mountPointFromDiskutil(disk.bsd), let url = usableMountURL(fromInfo) {
            return url
        }
        return usableMountURL("/Volumes/\(disk.title)")
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
        closeEngine(remountFinder: currentDisk != nil, restoreBrowse: false)
        remountGen = UUID()
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

    @discardableResult
    private func syncThenUmount(_ h: OpaquePointer) -> Bool {
        let ok = myntfs_sync(h) == 0
        if !ok { appendLog("flush before umount: \(lastErr())") }
        myntfs_umount(h)
        return ok
    }

    func close() {
        closeEngine(remountFinder: currentDisk != nil, restoreBrowse: true)
    }

    func closeVolume() {
        closeEngine(remountFinder: true, restoreBrowse: true)
    }

    func closeEngine(remountFinder: Bool, restoreBrowse: Bool = true) {
        Self.cleanupStaleOpenTemps()
        let disk = currentDisk ?? (remountFinder ? lastDisk : nil)
        let h = handle
        handle = nil
        engineWritable = false
        wantWrite = false
        deviceWriteConfirmed = false
        selectedName = nil
        cwd = "/"
        searchText = ""
        volumeTotal = 0
        volumeFree = 0
        if let u = scopedURL {
            u.stopAccessingSecurityScopedResource()
            scopedURL = nil
        }

        let gen = UUID()
        remountGen = gen
        if restoreBrowse, remountFinder, disk != nil {
            busy = true
            busyCancellable = false
            mutating = false
            busyMessage = "Saving changes so Windows and Finder can open the files…"
        }

        let remount = remountFinder
        let browse = restoreBrowse
        let capturedDisk = disk
        let work = {
            if let h {
                if myntfs_sync(h) != 0 {
                    self.appendLog("flush before close: \(self.lastErr())")
                }
                myntfs_umount(h)
            }
            myntfs_da_release()
            var spins = 0
            while myntfs_da_holding() != 0 && spins < 50 {
                Thread.sleep(forTimeInterval: 0.02)
                spins += 1
            }
            var folder: URL?
            if remount, let capturedDisk {
                folder = self.mountFinderPath(capturedDisk)
            }
            return folder
        }

        if remount, let capturedDisk {
            DispatchQueue.global(qos: .userInitiated).async {
                let folder = work()
                DispatchQueue.main.async {
                    guard self.remountGen == gen else { return }
                    if browse {
                        self.applyBrowseRestore(disk: capturedDisk, folder: folder)
                    }
                }
            }
            if !browse {
                hostBrowseRoot = nil
            }
            return
        }

        _ = work()
        hostBrowseRoot = nil
        currentDisk = nil
        entries = []
        volumeTitle = ""
        badge = .none
    }

    func applyBrowseRestore(disk: DetectedDisk, folder: URL?) {
        busy = false
        busyMessage = ""
        lastDisk = disk
        currentDisk = disk
        volumeTitle = disk.title
        engineWritable = false
        wantWrite = false
        deviceWriteConfirmed = false
        cwd = "/"
        selectedName = nil
        handle = nil
        if let folder {
            hostBrowseRoot = folder
            path = folder.path
            badge = .finderMount
            status = "Finder has \(folder.path) (read-only)"
            refresh()
            appendLog("Finder remounted \(folder.path) read-only")
        } else {
            hostBrowseRoot = nil
            entries = []
            badge = .none
            status = "Could not remount \(disk.title) in Finder. Open Disk Utility and mount \(disk.bsd)."
            appendLog(status)
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
        selectedName = nil
        refresh()
    }

    func goTo(_ path: String) {
        cwd = path.isEmpty ? "/" : path
        selectedName = nil
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
            rows.append(DirRow(path: joinPath(cwd, name), name: name, isDir: isDir[i] != 0, size: sizes[i]))
        }
        entries = rows
        status = "\(volumeTitle)\(cwd)  —  \(visibleEntries.count) items"
        refreshSpaceAsync()
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
                    path: url.path,
                    name: name,
                    isDir: vals?.isDirectory == true,
                    size: UInt64(vals?.fileSize ?? 0)
                )
            }
            .sorted {
                if $0.isDir != $1.isDir { return $0.isDir && !$1.isDir }
                return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
            }
            status = "\(volumeTitle)\(cwd)  —  \(visibleEntries.count) items"
        } catch {
            status = "Could not list \(folder.path): \(error.localizedDescription). Allow MyNTFS for Removable Volumes in System Settings if macOS asked."
            appendLog(status)
        }
    }

    func refreshSpaceAsync() {
        guard let h = handle, !busy else { return }
        DispatchQueue.global(qos: .utility).async {
            var total: UInt64 = 0
            var free: UInt64 = 0
            let rc = myntfs_volume_space(h, &total, &free)
            DispatchQueue.main.async {
                if rc == 0 {
                    self.volumeTotal = total
                    self.volumeFree = free
                }
            }
        }
    }

    func openFile(_ row: DirRow) {
        if let url = currentHostURL(row.name) {
            NSWorkspace.shared.open(url)
            appendLog("opened \(url.path)")
            return
        }
        guard handle != nil else { return }
        Self.cleanupStaleOpenTemps()
        let session = FileManager.default.temporaryDirectory
            .appendingPathComponent(Self.openTempFolder, isDirectory: true)
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: session, withIntermediateDirectories: true)
        } catch {
            appendLog("open temp: \(error.localizedDescription)")
            return
        }
        let dest = session.appendingPathComponent(row.name)
        copyOut(entry: row, to: dest)
        NSWorkspace.shared.open(dest)
    }

    func revealInFinder(_ row: DirRow) {
        guard usingHostBrowse, !canMutate, myntfs_da_holding() == 0 else { return }
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
        guard let h = handle else { return }
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

    var visibleEntries: [DirRow] {
        var rows = entries
        if hideSystemFiles {
            rows = rows.filter { row in
                !row.name.hasPrefix("$") && row.name != "System Volume Information"
            }
        }
        let q = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        if !q.isEmpty {
            rows = rows.filter { $0.name.localizedCaseInsensitiveContains(q) }
        }
        rows.sort { a, b in
            switch sortMode {
            case .name:
                if a.isDir != b.isDir { return a.isDir && !b.isDir }
                return a.name.localizedCaseInsensitiveCompare(b.name) == .orderedAscending
            case .size:
                if a.isDir != b.isDir { return a.isDir && !b.isDir }
                if a.size != b.size { return a.size > b.size }
                return a.name.localizedCaseInsensitiveCompare(b.name) == .orderedAscending
            case .kind:
                if a.isDir != b.isDir { return a.isDir && !b.isDir }
                let ae = (a.name as NSString).pathExtension
                let be = (b.name as NSString).pathExtension
                if ae != be { return ae.localizedCaseInsensitiveCompare(be) == .orderedAscending }
                return a.name.localizedCaseInsensitiveCompare(b.name) == .orderedAscending
            }
        }
        return rows
    }

    var pathCrumbs: [(label: String, path: String)] {
        let root = volumeTitle.isEmpty ? "Volume" : volumeTitle
        var crumbs = [(root, "/")]
        let parts = cwd.split(separator: "/").map(String.init)
        var acc = ""
        for part in parts {
            acc += "/\(part)"
            crumbs.append((part, acc))
        }
        return crumbs
    }

    var spaceCaption: String {
        if volumeTotal == 0 { return "" }
        let free = ByteCountFormatter.string(fromByteCount: Int64(volumeFree), countStyle: .file)
        let total = ByteCountFormatter.string(fromByteCount: Int64(volumeTotal), countStyle: .file)
        return "\(free) free of \(total)"
    }

    func lastErr() -> String {
        Self.presentableEngineError(String(cString: myntfs_last_error()))
    }

    static func presentableEngineError(_ raw: String) -> String {
        let lower = raw.lowercased()
        if lower.contains("exceeds record capacity")
            || lower.contains("no indx block")
            || lower.contains("directory index")
            || lower.contains("insert index entry") {
            return "This folder is full. NTFS can only store so many names in one folder on this volume. Create a new folder and drop fewer items."
        }
        if lower.contains("is not empty") || lower.contains("index_allocation overflow") {
            return "Could not delete this folder because something inside it could not be removed."
        }
        return raw
    }

    func copyNtfsPath() {
        guard let row = selectedRow else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(currentNtfsPath(row.name), forType: .string)
        appendLog("copied path \(currentNtfsPath(row.name))")
    }

    func copyName() {
        guard let row = selectedRow else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(row.name, forType: .string)
    }

    func uniqueCopyName(_ name: String) -> String {
        let ns = name as NSString
        let ext = ns.pathExtension
        let stem = ext.isEmpty ? name : ns.deletingPathExtension
        var candidate = ext.isEmpty ? "\(stem) copy" : "\(stem) copy.\(ext)"
        var i = 2
        while entries.contains(where: { $0.name.caseInsensitiveCompare(candidate) == .orderedSame }) {
            candidate = ext.isEmpty ? "\(stem) copy \(i)" : "\(stem) copy \(i).\(ext)"
            i += 1
        }
        return candidate
    }

    func duplicateSelected() {
        guard ensureWritable(), let row = selectedRow else { return }
        let newName = uniqueCopyName(row.name)
        let srcPath = currentNtfsPath(row.name)
        runEngineWork(message: "Duplicating \(row.name)…") { h in
            let tmp = FileManager.default.temporaryDirectory
                .appendingPathComponent("MyNTFS-dup-\(UUID().uuidString)", isDirectory: true)
            do {
                try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
            } catch {
                return error.localizedDescription
            }
            let dest = tmp.appendingPathComponent(row.name)
            if myntfs_copy_out(h, srcPath, dest.path) < 0 {
                return self.lastErr()
            }
            if myntfs_copy_in(h, dest.path, self.cwd, newName) < 0 {
                return self.lastErr()
            }
            try? FileManager.default.removeItem(at: tmp)
            return nil
        } onSuccess: {
            self.appendLog("duplicated \(row.name) → \(newName)")
            self.selectedName = newName
        }
    }

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
            return myntfs_remove(h, path) != 0 ? self.lastErr() : nil
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
        importHostItems([url])
    }

    func presentImportPanel(foldersOnly: Bool) {
        guard ensureWritable() else { return }
        let panel = NSOpenPanel()
        panel.canChooseFiles = !foldersOnly
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.treatsFilePackagesAsDirectories = true
        panel.message = foldersOnly
            ? "Choose folders to copy onto this NTFS volume."
            : "Choose files or folders to copy onto this NTFS volume."
        panel.prompt = "Add"
        guard panel.runModal() == .OK else { return }
        importHostItems(panel.urls)
    }

    func handleDrop(_ urls: [URL]) -> Bool {
        let resolved = urls.map { $0.standardizedFileURL }
        guard !resolved.isEmpty else { return false }
        if !isMounted {
            if let img = resolved.first(where: isDiskImageURL) {
                wantWrite = false
                deviceWriteConfirmed = false
                open(url: img, requestWrite: false, allowDeviceWrite: false)
                return true
            }
            actionError = "Open a USB volume or drop an NTFS disk image (.img) first."
            return false
        }
        if !canMutate {
            showEnableWrite = true
            return false
        }
        importHostItems(resolved)
        return true
    }

    func handleExternalURL(_ url: URL) {
        _ = handleDrop([url])
    }

    func isDiskImageURL(_ url: URL) -> Bool {
        let ext = url.pathExtension.lowercased()
        return ["img", "dmg", "raw", "iso", "bin"].contains(ext)
    }

    func importHostItems(_ urls: [URL]) {
        guard ensureWritable() else { return }
        let scoped = urls
        runEngineWork(message: "Copying from Mac…") { h in
            for url in scoped {
                let accessed = url.startAccessingSecurityScopedResource()
                defer {
                    if accessed { url.stopAccessingSecurityScopedResource() }
                }
                if myntfs_copy_in(h, url.path, self.cwd, url.lastPathComponent) < 0 {
                    return self.lastErr()
                }
            }
            return nil
        } onSuccess: {
            self.appendLog("imported \(urls.map(\.lastPathComponent).joined(separator: ", "))")
            if let last = urls.last {
                self.selectedName = last.lastPathComponent
            }
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
        if safety.bitlocker {
            actionError = "Write blocked: BitLocker encryption was detected. Decrypt the volume in Windows first."
            return
        }
        guard let disk = currentDisk ?? disks.first(where: { $0.title == volumeTitle }) else {
            actionError = "No disk selected."
            return
        }
        guard disk.isNtfs else {
            actionError = "\(disk.title) is \(disk.fileSystem). Enable writes works on NTFS only."
            return
        }
        guard !busy else { return }
        busy = true
        busyCancellable = true
        mutating = false
        busyMessage = "Disconnecting from Finder so MyNTFS can write…"
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
            myntfs_da_release()
            var folder: URL?
            if remount {
                folder = self.mountFinderPath(disk)
            }
            DispatchQueue.main.async {
                guard self.workGen == gen else { return }
                self.busy = false
                self.busyCancellable = false
                self.mutating = false
                self.busyMessage = ""
                self.appendLog(message)
                if remount {
                    self.applyBrowseRestore(disk: disk, folder: folder)
                }
                self.actionError = message
            }
        }

        func cancelledRestore() {
            myntfs_da_release()
            let folder = self.mountFinderPath(disk)
            DispatchQueue.main.async {
                guard self.workGen == gen else { return }
                self.applyBrowseRestore(disk: disk, folder: folder)
            }
        }

        if cancelled() {
            return
        }

        var err = [CChar](repeating: 0, count: 512)
        let holdRc = myntfs_da_hold(disk.bsd, &err, err.count)
        if cancelled() {
            cancelledRestore()
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
            cancelledRestore()
            return
        }

        DispatchQueue.main.async {
            guard self.workGen == gen else { return }
            self.busyMessage = "macOS is asking for permission to open the disk…"
        }
        let fd = openWritableRdisk(disk.rdisk)
        if cancelled() {
            if fd >= 0 { Darwin.close(fd) }
            cancelledRestore()
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
            if cancelled() {
                cancelledRestore()
                return
            }
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
            cancelledRestore()
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
            cancelledRestore()
            return
        }
        let mounted = myntfs_mount_fd(fd, disk.rdisk, 1, 1, &err, err.count)
        Darwin.close(fd)
        if cancelled() {
            if let mounted { syncThenUmount(mounted) }
            cancelledRestore()
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
                self.syncThenUmount(mounted)
                DispatchQueue.global(qos: .userInitiated).async {
                    myntfs_da_release()
                    let folder = self.mountFinderPath(disk)
                    DispatchQueue.main.async {
                        self.applyBrowseRestore(disk: disk, folder: folder)
                    }
                }
                return
            }
            self.busy = false
            self.busyCancellable = false
            self.mutating = false
            self.busyMessage = ""
            if let old = self.handle {
                self.syncThenUmount(old)
            }
            self.hostBrowseRoot = nil
            self.handle = mounted
            self.currentDisk = disk
            self.lastDisk = disk
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
        if t.contains("another driver") {
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
        busyCancellable = false
        busyMessage = "Returning the drive to Finder…"
        myntfs_da_release()
        appendLog("enable-writes cancelled; releasing exclusive access")
        guard let disk = currentDisk ?? lastDisk ?? disks.first(where: { $0.title == volumeTitle }) else {
            busy = false
            busyMessage = ""
            return
        }
        DispatchQueue.global(qos: .userInitiated).async {
            let folder = self.mountFinderPath(disk)
            DispatchQueue.main.async {
                self.applyBrowseRestore(disk: disk, folder: folder)
            }
        }
    }
}

struct DirRow: Identifiable, Hashable {
    let path: String
    let name: String
    let isDir: Bool
    let size: UInt64

    var id: String { path }

    var kindLabel: String {
        if isDir { return "Folder" }
        let ext = (name as NSString).pathExtension
        return ext.isEmpty ? "File" : "\(ext.uppercased()) file"
    }

    var symbolName: String {
        if isDir { return "folder.fill" }
        switch (name as NSString).pathExtension.lowercased() {
        case "txt", "md", "log", "csv": return "doc.plaintext.fill"
        case "py", "rs", "c", "h", "swift", "js", "ts", "go", "java": return "chevron.left.forwardslash.chevron.right"
        case "png", "jpg", "jpeg", "gif", "webp", "svg": return "photo"
        case "zip", "gz", "tar", "7z": return "archivebox.fill"
        case "pdf": return "doc.richtext"
        case "sh", "command", "bat", "ps1": return "terminal.fill"
        case "yml", "yaml", "json", "toml", "xml": return "doc.text.fill"
        default: return "doc.fill"
        }
    }
}

struct ContentView: View {
    @EnvironmentObject var model: VolumeModel
    @StateObject private var _showLogExport = StateObject(initialValue: false)
    private var showLogExport: Bool {
        get { _showLogExport.wrappedValue }
        set { _showLogExport.wrappedValue = newValue }
    }
    @State private var copyTarget: DirRow?
    @State private var showCopySave = false
    @State private var dropTargeted = false

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
                        Text("Plug in a USB drive")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(model.disks) { disk in
                        Button {
                            model.openDisk(disk)
                        } label: {
                            HStack(spacing: 10) {
                                Image(systemName: disk.isNtfs ? "externaldrive.fill" : "externaldrive")
                                    .foregroundStyle(disk.isNtfs ? Color.accentColor : Color.secondary)
                                    .font(.title3)
                                VStack(alignment: .leading, spacing: 2) {
                                    HStack(spacing: 6) {
                                        Text(disk.title)
                                            .fontWeight(model.currentDisk?.id == disk.id ? .semibold : .regular)
                                        if !disk.isNtfs {
                                            Text(disk.fileSystem.isEmpty ? "not NTFS" : disk.fileSystem)
                                                .font(.caption2.weight(.semibold))
                                                .padding(.horizontal, 6)
                                                .padding(.vertical, 1)
                                                .background(Color.secondary.opacity(0.18))
                                                .clipShape(Capsule())
                                        }
                                    }
                                    Text(disk.subtitle)
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                            }
                            .padding(.vertical, 2)
                        }
                        .buttonStyle(.plain)
                        .help(disk.isNtfs ? "Open this NTFS volume" : "\(disk.fileSystem) — MyNTFS opens NTFS only")
                        .accessibilityLabel(disk.accessibilityTitle)
                    }
                    Button {
                        model.refreshDisks()
                    } label: {
                        Label("Scan disks", systemImage: "arrow.clockwise")
                    }
                    .disabled(model.scanning || model.busy)
                    .accessibilityLabel("Scan disks")
                    Button {
                        model.openImagePanel()
                    } label: {
                        Label("Open image…", systemImage: "internaldrive")
                    }
                    .disabled(model.busy)
                    .accessibilityLabel("Open disk image")
                }
            }
            .navigationTitle("MyNTFS")
            .listStyle(.sidebar)
            .navigationSplitViewColumnWidth(min: 220, ideal: 260)
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
            .background(dropTargeted ? Color.accentColor.opacity(0.12) : Color.clear)
            .dropDestination(for: URL.self) { urls, _ in
                model.handleDrop(urls)
            } isTargeted: { dropTargeted = $0 }
        }
        .onReceive(NotificationCenter.default.publisher(for: .myntfsExportLog)) { _ in
            showLogExport = true
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
                                .accessibilityLabel("Cancel")
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
            if model.selectedRow?.isDir == true {
                Text("This deletes the folder and everything inside it. This cannot be undone.")
            } else {
                Text("This cannot be undone.")
            }
        }
        .alert("Enable writes on \(model.volumeTitle.isEmpty ? "this disk" : model.volumeTitle)?\n\nWarning: Experimental writes may corrupt or destroy your data.", isPresented: $model.showEnableWrite) {
            Button("Cancel", role: .cancel) {}
            if !(model.safety.bitlocker && !model.enableWritesIsImage) {
                Button("Enable writes", role: .destructive) { model.enableWrites() }
            }
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
        .sheet(isPresented: $model.showGetInfo) {
            getInfoSheet
        }
    }

    @ViewBuilder
    private var getInfoSheet: some View {
        let row = model.selectedRow
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 12) {
                Image(systemName: row?.symbolName ?? "doc")
                    .font(.system(size: 36))
                    .foregroundStyle(row?.isDir == true ? Color.accentColor : Color.secondary)
                VStack(alignment: .leading, spacing: 4) {
                    Text(row?.name ?? "Nothing selected")
                        .font(.title3.weight(.semibold))
                    Text(row?.kindLabel ?? "")
                        .foregroundStyle(.secondary)
                }
            }
            if let row {
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 8) {
                    GridRow {
                        Text("Path").foregroundStyle(.secondary)
                        Text(model.currentNtfsPath(row.name)).textSelection(.enabled)
                    }
                    GridRow {
                        Text("Size").foregroundStyle(.secondary)
                        Text(row.isDir ? "Folder" : ByteCountFormatter.string(fromByteCount: Int64(row.size), countStyle: .file))
                    }
                    GridRow {
                        Text("Modified").foregroundStyle(.secondary)
                        Text(model.formattedMtime(for: row)).textSelection(.enabled)
                    }
                    GridRow {
                        Text("On volume").foregroundStyle(.secondary)
                        Text(model.volumeTitle.isEmpty ? "—" : model.volumeTitle)
                    }
                }
                .font(.callout)
            }
            Spacer()
            HStack {
                if row != nil {
                    Button("Copy Path") { model.copyNtfsPath() }
                }
                Spacer()
                Button("Done") { model.showGetInfo = false }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(minWidth: 420, minHeight: 220)
    }

    private func presentCopyToMac(_ row: DirRow) {
        if row.isDir {
            let panel = NSOpenPanel()
            panel.canChooseDirectories = true
            panel.canChooseFiles = false
            panel.canCreateDirectories = true
            panel.prompt = "Copy here"
            panel.message = "Choose a folder on this Mac to receive “\(row.name)”."
            guard panel.runModal() == .OK, let folder = panel.url else { return }
            model.copyOut(entry: row, to: folder.appendingPathComponent(row.name))
        } else {
            copyTarget = row
            showCopySave = true
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
                "Finder will disappear for this USB until you Close volume. That is exclusive access, not a crash.",
                "macOS will ask for your admin password once for this Enable writes. MyNTFS does not keep that authorization.",
                "Do not do this if you cannot replace the files on the drive."
            ]
        }
        let safety = model.safety.summaryLines.filter { $0 != "No safety warnings" }
        if !safety.isEmpty {
            lines.append(contentsOf: safety.map { "• \($0)" })
        }
        if model.safety.bitlocker && !model.enableWritesIsImage {
            lines.append("BitLocker is present. Enable writes is disabled until you decrypt in Windows.")
        }
        return lines.joined(separator: "\n\n")
    }

    private var explorerBar: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Button { model.goUp() } label: {
                    Image(systemName: "chevron.left")
                }
                .disabled(!model.canGoUp)
                .help("Enclosing folder (⌘↑)")
                .accessibilityLabel("Enclosing folder")

                Button { model.goHome() } label: {
                    Image(systemName: "house")
                }
                .disabled(!model.isMounted)
                .help("Volume root")
                .accessibilityLabel("Volume root")

                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 4) {
                        if model.isMounted {
                            ForEach(Array(model.pathCrumbs.enumerated()), id: \.offset) { i, crumb in
                                if i > 0 {
                                    Image(systemName: "chevron.right")
                                        .font(.caption2)
                                        .foregroundStyle(.tertiary)
                                }
                                Button(crumb.label) { model.goTo(crumb.path) }
                                    .buttonStyle(.plain)
                                    .font(i == model.pathCrumbs.count - 1 ? .body.weight(.semibold) : .body)
                            }
                        } else {
                            Text("No volume")
                                .foregroundStyle(.secondary)
                        }
                    }
                }

                Spacer(minLength: 8)

                Text(model.badge.rawValue)
                    .font(.caption.bold())
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(model.badge.color.opacity(0.18))
                    .foregroundStyle(model.badge.color)
                    .clipShape(Capsule())

                if !model.canMutate && model.isMounted {
                    Button("Enable writes…") { model.showEnableWrite = true }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .disabled(model.busy || (model.safety.bitlocker && !model.enableWritesIsImage))
                        .accessibilityLabel("Enable writes")
                        .accessibilityHint("Ask macOS for exclusive access so MyNTFS can write")
                }
                Button("Close") { model.closeVolume() }
                    .disabled(!model.isMounted || model.busy)
                    .help("Flush, reset the NTFS log, and return the drive to Finder")
                    .accessibilityLabel("Close volume")
                    .accessibilityHint("Flush, reset the NTFS log, and return the drive to Finder")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)

            HStack(spacing: 8) {
                Button { model.beginNewFolder() } label: { Label("Folder", systemImage: "folder.badge.plus") }
                    .disabled(!model.canMutate || model.busy)
                    .accessibilityLabel("New folder")
                Button { model.presentImportPanel(foldersOnly: true) } label: { Label("Add folder", systemImage: "plus.rectangle.on.folder") }
                    .disabled(!model.canMutate || model.busy)
                    .accessibilityLabel("Add folder from this Mac")
                Button { model.presentImportPanel(foldersOnly: false) } label: { Label("Import", systemImage: "square.and.arrow.down") }
                    .disabled(!model.canMutate || model.busy)
                    .accessibilityLabel("Import from this Mac")
                Button { model.beginRename() } label: { Label("Rename", systemImage: "pencil") }
                    .disabled(!model.canMutate || model.selectedRow == nil || model.busy)
                    .accessibilityLabel("Rename selected item")
                Button { model.duplicateSelected() } label: { Label("Duplicate", systemImage: "plus.square.on.square") }
                    .disabled(!model.canMutate || model.selectedRow == nil || model.busy)
                    .accessibilityLabel("Duplicate selected item")
                Button { model.showGetInfo = true } label: { Label("Info", systemImage: "info.circle") }
                    .disabled(model.selectedRow == nil)
                    .accessibilityLabel("Get Info")
                Button("Delete", role: .destructive) { model.deleteSelected() }
                    .disabled(!model.canMutate || model.selectedRow == nil || model.busy)
                    .accessibilityLabel("Delete selected item")
                Spacer()
                Picker("Sort", selection: $model.sortMode) {
                    ForEach(VolumeModel.SortMode.allCases) { mode in
                        Text(mode.rawValue).tag(mode)
                    }
                }
                .pickerStyle(.menu)
                .frame(width: 110)
                Toggle("Hide $", isOn: $model.hideSystemFiles)
                    .toggleStyle(.checkbox)
                    .help("Hide NTFS metadata such as $MFT")
                TextField("Search", text: $model.searchText)
                    .textFieldStyle(.roundedBorder)
                    .frame(maxWidth: 180)
            }
            .controlSize(.small)
            .padding(.horizontal, 12)
            .padding(.bottom, 8)
        }
    }

    private var fileList: some View {
        VStack(spacing: 0) {
            if model.canMutate {
                HStack(spacing: 8) {
                    Image(systemName: "lock.open.fill").foregroundStyle(.green)
                    Text("Exclusive access. Finder is hidden for this drive until you Close. Drop files here to copy them in.")
                        .font(.caption)
                    Spacer()
                }
                .padding(8)
                .background(Color.green.opacity(0.12))
            } else if model.isMounted {
                HStack(spacing: 8) {
                    Image(systemName: "lock.fill").foregroundStyle(.orange)
                    Text("Read-only. Apple’s NTFS driver cannot create or delete files.")
                        .font(.caption)
                    Spacer()
                    Button("Enable writes…") { model.showEnableWrite = true }
                        .controlSize(.small)
                        .disabled(model.busy || (model.safety.bitlocker && !model.enableWritesIsImage))
                        .accessibilityLabel("Enable writes")
                        .accessibilityHint("Ask macOS for exclusive access so MyNTFS can write")
                }
                .padding(8)
                .background(Color.orange.opacity(0.12))
            }
            ZStack {
                if model.visibleEntries.isEmpty {
                    VStack(spacing: 10) {
                        Image(systemName: model.canMutate ? "folder.badge.plus" : "folder")
                            .font(.system(size: 36))
                            .foregroundStyle(.secondary)
                        Text(emptyFolderMessage)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 24)
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
                List(selection: $model.selectedName) {
                    ForEach(model.visibleEntries) { row in
                        HStack(spacing: 10) {
                            Image(systemName: row.symbolName)
                                .foregroundStyle(row.isDir ? Color.accentColor : Color.secondary)
                                .frame(width: 22)
                            Text(row.name)
                                .lineLimit(1)
                            Spacer()
                            Text(row.kindLabel)
                                .font(.caption)
                                .foregroundStyle(.tertiary)
                                .frame(width: 88, alignment: .trailing)
                            Text(row.isDir ? "—" : ByteCountFormatter.string(fromByteCount: Int64(row.size), countStyle: .file))
                                .foregroundStyle(.secondary)
                                .font(.caption.monospacedDigit())
                                .frame(width: 88, alignment: .trailing)
                        }
                        .tag(row.name)
                        .contentShape(Rectangle())
                        .onTapGesture(count: 2) { model.activate(row) }
                        .contextMenu { rowMenu(row) }
                        .accessibilityElement(children: .combine)
                        .accessibilityLabel("\(row.name), \(row.kindLabel)")
                    }
                }
                .listStyle(.inset)
                .opacity(model.visibleEntries.isEmpty ? 0.01 : 1)
                .contextMenu {
                    Button("New Folder…") { model.beginNewFolder() }
                        .disabled(!model.canMutate)
                    Button("Add Folder from Mac…") { model.presentImportPanel(foldersOnly: true) }
                        .disabled(!model.canMutate)
                    Button("Import from Mac…") { model.presentImportPanel(foldersOnly: false) }
                        .disabled(!model.canMutate)
                    Button("New File…") { model.beginNewFile() }
                        .disabled(!model.canMutate)
                }
                .onDeleteCommand { model.deleteSelected() }
            }
            HStack {
                Text(model.status)
                if !model.spaceCaption.isEmpty {
                    Text("·")
                    Text(model.spaceCaption)
                }
                if !model.searchText.isEmpty {
                    Text("·")
                    Text("filter “\(model.searchText)”")
                }
                Spacer()
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
        }
    }

    private var emptyFolderMessage: String {
        if !model.searchText.isEmpty {
            return "No items match “\(model.searchText)”."
        }
        if model.hideSystemFiles && !model.entries.isEmpty {
            return "Only NTFS system files are here. Uncheck Hide $ to show them."
        }
        if model.canMutate {
            return "This folder is empty. Drop files or folders here, or use Add folder / Import."
        }
        return "This folder is empty."
    }

    @ViewBuilder
    private func rowMenu(_ row: DirRow) -> some View {
        Button("Open") { model.activate(row) }
        Button("Get Info") {
            model.selectedName = row.name
            model.showGetInfo = true
        }
        Button("Copy to Mac…") {
            model.selectedName = row.name
            presentCopyToMac(row)
        }
        Button("Copy Path") {
            model.selectedName = row.name
            model.copyNtfsPath()
        }
        if model.usingHostBrowse && !model.canMutate {
            Button("Reveal in Finder") { model.revealInFinder(row) }
        }
        if model.canMutate {
            Divider()
            if !row.isDir {
                Button("Edit…") {
                    model.selectedName = row.name
                    model.editSelected()
                }
            }
            Button("Duplicate") {
                model.selectedName = row.name
                model.duplicateSelected()
            }
            Button("Rename…") {
                model.selectedName = row.name
                model.beginRename()
            }
            Button("Delete…", role: .destructive) {
                model.selectedName = row.name
                model.deleteSelected()
            }
        } else if model.isMounted {
            Divider()
            Button("Enable writes…") { model.showEnableWrite = true }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 20) {
            Image(systemName: "externaldrive.fill")
                .font(.system(size: 52))
                .foregroundStyle(.tint)
            Text("MyNTFS")
                .font(.largeTitle.weight(.semibold))
            Text(model.status)
                .font(.headline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal)
            if model.scanning {
                ProgressView("Scanning for USB drives…")
            }
            if let disk = model.currentDisk, !disk.isNtfs {
                VStack(spacing: 10) {
                    Text("\(disk.title) is \(disk.fileSystem)")
                        .font(.title3.weight(.semibold))
                    Text("MyNTFS reads and writes NTFS. Use this stick from Finder, or format it as NTFS in Windows if you want it here.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 420)
                    if !disk.mountPoint.isEmpty {
                        Button("Reveal in Finder") { model.revealInFinder(disk) }
                            .buttonStyle(.borderedProminent)
                    }
                }
                .padding(.top, 4)
            } else if !model.disks.isEmpty {
                VStack(spacing: 10) {
                    ForEach(model.disks) { disk in
                        Button {
                            model.openDisk(disk)
                        } label: {
                            VStack(alignment: .leading, spacing: 4) {
                                Text(disk.isNtfs ? "Explore \(disk.title)" : "\(disk.title) · \(disk.fileSystem)")
                                    .font(.headline)
                                Text(disk.isNtfs ? disk.subtitle : "Not NTFS — MyNTFS will not open this volume")
                                    .font(.caption)
                            }
                            .frame(maxWidth: 360, alignment: .leading)
                            .padding(.vertical, 6)
                        }
                        .buttonStyle(.bordered)
                        .tint(disk.isNtfs ? Color.accentColor : Color.secondary)
                        .disabled(model.busy)
                        .accessibilityLabel(disk.accessibilityTitle)
                    }
                }
            }
            HStack(spacing: 12) {
                Button("Scan disks") { model.refreshDisks() }
                    .disabled(model.scanning || model.busy)
                    .accessibilityLabel("Scan disks")
                Button("Open disk image…") { model.openImagePanel() }
                    .disabled(model.busy)
                    .accessibilityLabel("Open disk image")
            }
            Text("Drop an .img here, or drag files and folders onto an open read-write volume.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
    }
}

extension Notification.Name {
    static let myntfsExportLog = Notification.Name("myntfsExportLog")
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
