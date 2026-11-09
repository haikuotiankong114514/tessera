import AppKit
import Combine
import Darwin
import Foundation

func unpack(_ buffer: TSBuffer) -> Data {
  defer { ts_free(buffer) }
  guard let pointer = buffer.ptr, buffer.len > 0 else { return Data() }
  return Data(bytes: pointer, count: buffer.len)
}
func decode<T: Decodable>(_ type: T.Type, _ buffer: TSBuffer) throws -> T {
  try JSONDecoder().decode(type, from: unpack(buffer))
}
final class CoreHandle: @unchecked Sendable {
  let pointer: UnsafeMutableRawPointer
  init(url: URL, threads: UInt32 = 0) {
    let path = url.withUnsafeFileSystemRepresentation { p in
      Array(
        UnsafeBufferPointer(
          start: UnsafeRawPointer(p!).assumingMemoryBound(to: UInt8.self), count: strlen(p!)))
    }
    pointer = path.withUnsafeBufferPointer { ts_create($0.baseAddress, $0.count, threads)! }
  }
  func cancel() { ts_cancel(pointer) }
  deinit {
    ts_cancel(pointer)
    ts_release(pointer)
  }
}
struct ScanStatus: Decodable {
  var files: UInt64 = 0, dirs: UInt64 = 0, scanned: UInt64 = 0, logical: UInt64 = 0
  var errors: UInt64 = 0, nodes: UInt64 = 0, treeBytes: UInt64 = 0, elapsedMs: UInt64 = 0
  var done = false
  var rev: UInt32 = 0
  var error: String?
  var capacity: UInt64 = 0, free: UInt64 = 0
}
struct FileIdentity: Codable, Hashable {
  let device: Int32
  let inode: UInt64
}
struct Entry: Codable, Identifiable, Hashable {
  let id: UInt32
  let name: String
  let path: String
  let pathBytes: [UInt8]
  let disk: UInt64
  let logical: UInt64
  let dir: Bool
  let flags: UInt16
  let modified: Int64?
  let lockReason: String?
  let files: UInt64
  let dirs: UInt64
  let parent: UInt32?
  let identity: FileIdentity?
  var url: URL {
    var terminated = pathBytes.map { CChar(bitPattern: $0) }
    terminated.append(0)
    return terminated.withUnsafeBufferPointer {
      URL(fileURLWithFileSystemRepresentation: $0.baseAddress!, isDirectory: dir, relativeTo: nil)
    }
  }
}
struct Nav: Decodable {
  struct Crumb: Decodable {
    let id: UInt32
    let name: String
  }
  let up: UInt32?
  let crumbs: [Crumb]
}
struct CategoryTotal: Decodable, Identifiable {
  let id: Int
  let bytes: UInt64
  var label: String {
    let names = [
      ("Other", "其他"), ("Video", "视频"), ("Images", "图像"), ("Audio", "音频"), ("Archives", "压缩包"),
      ("Disk images", "磁盘映像"), ("Apps", "应用"), ("Games", "游戏"), ("Documents", "文档"),
      ("Development", "开发"), ("Cache", "缓存"), ("System", "系统"),
    ]
    guard names.indices.contains(id) else { return tr("Other", "其他") }
    return tr(names[id].0, names[id].1)
  }
}
struct Volume: Identifiable {
  let url: URL
  let name: String
  let capacity: Int64
  let free: Int64
  var id: URL { url }
}
func sizeText(_ bytes: UInt64) -> String {
  ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .file)
}
func tr(_ en: String, _ zh: String) -> String {
  Locale.preferredLanguages.first?.hasPrefix("zh") == true ? zh : en
}
func reasonText(_ key: String) -> String {
  switch key {
  case "staleTarget":
    return tr(
      "The file changed since the scan. Refresh and select it again.", "文件在扫描后发生变化，请刷新并重新选择。")
  case "scanning": return tr("Wait for the scan to finish.", "请等待扫描结束。")
  case "homeRoot": return tr("The home folder itself cannot be deleted.", "不能删除用户主目录本身。")
  case "volumeRoot", "systemRoot", "systemProtected", "readOnly":
    return tr("This location is protected or read-only.", "此位置受保护或只读。")
  case "cancelled": return tr("Scan stopped — partial results", "扫描已停止 — 当前为部分结果")
  default: return key
  }
}

struct RenderStatus: Decodable {
  let gpu: Bool
  let frameMs: Double
  let fps: Int
  let rects: Int
  let busy: Bool
}

@MainActor final class Store: NSObject, ObservableObject {
  @Published var showRenderStatus = false
  @Published var renderStatus: RenderStatus?
  @Published var accessSetupPresented = false
  @Published var accessSetupBusy = false
  @Published var grantedFolderCount = 0
  @Published var root: URL?
  @Published var status = ScanStatus()
  @Published var generation: UInt32 = 0
  @Published var view: UInt32 = 0
  @Published var logical = false
  @Published var selection = Set<UInt32>()
  @Published var selected: Entry?
  @Published var entries = [Entry]()
  @Published var listVisible = false
  @Published var inspectorVisible = true
  @Published var volumes = [Volume]()
  @Published var categories = [CategoryTotal]()
  @Published var categoryFilter: Int?
  @Published var crumbs = [Nav.Crumb]()
  @Published var up: UInt32?
  @Published var notice: String?
  @Published var operation = false
  @Published var active = true
  var handle: CoreHandle?
  let worker = DispatchQueue(
    label: "io.github.haikuotiankong114514.tessera.core", qos: .userInitiated)
  var render: (() -> Void)?
  var webReady = false
  var monitor: ChangeMonitor?
  var timer: Timer?
  var dirty = Set<URL>()
  var dirtyWork: DispatchWorkItem?
  var history: [UInt32] = []
  var forward: [UInt32] = []
  var undoBatches = [[(original: URL, trash: URL, identity: FileIdentity)]]()
  var lastProgress: UInt64 = 0
  var scanFrames: UInt32 = 0
  private struct CachedScan {
    let root: URL
    let core: CoreHandle
    let status: ScanStatus
    let monitor: ChangeMonitor
    let watchID: UInt32
    let expires: TimeInterval
    var dirty: Set<URL>
  }

  private static let cacheLifetime: TimeInterval = 30
  private static let maxCachedTreeBytes: UInt64 = 160 * 1024 * 1024
  private static let maxCachedChanges = 128
  private var cachedScan: CachedScan?
  private var cacheExpiry: DispatchWorkItem?
  private var watchID: UInt32?
  private var refreshBusy = false
  private var cacheEligible = true
  var pollBusy = false
  var isScanning: Bool { root != nil && !status.done }
  var canDelete: Bool { !selection.isEmpty && !isScanning && !operation }
  var canUndo: Bool { !undoBatches.isEmpty && !operation && !isScanning }
  private let requireAccessSetup: Bool
  private let preferences: UserDefaults
  private var pendingScan: URL?
  private var grantedFolders: [URL] = []
  private static let accessSetupKey = "tessera.accessSetupCompleted.v1"
  private static let folderBookmarksKey = "tessera.grantedFolders.v1"

  init(requireAccessSetup: Bool = false, preferences: UserDefaults = .standard) {
    self.requireAccessSetup = requireAccessSetup
    self.preferences = preferences
    super.init()
    if requireAccessSetup {
      restoreFolderAccess()
      accessSetupPresented = !preferences.bool(forKey: Self.accessSetupKey)
    }
    reloadVolumes()
    NotificationCenter.default.addObserver(
      forName: NSWorkspace.didMountNotification, object: nil, queue: .main
    ) { [weak self] _ in Task { @MainActor in self?.reloadVolumes() } }
    for name in [NSWindow.didMiniaturizeNotification, NSWindow.didDeminiaturizeNotification] {
      NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) {
        [weak self] _ in
        Task { @MainActor in self?.changed() }
      }
    }
    let center = NSWorkspace.shared.notificationCenter
    center.addObserver(forName: NSWorkspace.didMountNotification, object: nil, queue: .main) {
      [weak self] _ in Task { @MainActor in self?.reloadVolumes() }
    }
    center.addObserver(forName: NSWorkspace.didUnmountNotification, object: nil, queue: .main) {
      [weak self] _ in
      Task { @MainActor in
        self?.discardCache()
        self?.reloadVolumes()
      }
    }
    center.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) {
      [weak self] _ in
      Task { @MainActor in
        self?.discardCache()
        self?.rescan()
      }
    }
    NotificationCenter.default.addObserver(
      forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
    ) { [weak self] _ in
      Task { @MainActor in
        self?.active = true
        self?.changed()
        self?.flushDirty()
      }
    }
    NotificationCenter.default.addObserver(
      forName: NSApplication.didResignActiveNotification, object: nil, queue: .main
    ) { [weak self] _ in
      Task { @MainActor in
        self?.active = false
        self?.changed()
      }
    }
  }
  func reloadVolumes() {
    let keys: [URLResourceKey] = [
      .volumeNameKey, .volumeTotalCapacityKey, .volumeAvailableCapacityKey, .volumeIsBrowsableKey,
      .volumeIsLocalKey,
    ]
    let urls =
      FileManager.default.mountedVolumeURLs(
        includingResourceValuesForKeys: keys, options: [.skipHiddenVolumes]) ?? []
    volumes = urls.compactMap { url in
      guard let v = try? url.resourceValues(forKeys: Set(keys)), v.volumeIsBrowsable != false,
        v.volumeIsLocal != false
      else { return nil }
      return Volume(
        url: url, name: v.volumeName ?? url.lastPathComponent,
        capacity: Int64(v.volumeTotalCapacity ?? 0), free: Int64(v.volumeAvailableCapacity ?? 0))
    }
  }
  func choose() {
    let panel = NSOpenPanel()
    panel.canChooseDirectories = true
    panel.canChooseFiles = false
    panel.allowsMultipleSelection = false
    panel.prompt = tr("Scan", "扫描")
    panel.message = tr("Choose a disk or folder to analyze.", "选择要分析的磁盘或文件夹。")
    panel.begin { [weak self] response in
      guard response == .OK, let url = panel.url else { return }
      self?.start(url)
    }
  }
  private func discardCache() {
    cacheExpiry?.cancel()
    cacheExpiry = nil
    cachedScan = nil
  }

  private func takeCache(for url: URL) -> CachedScan? {
    guard let snapshot = cachedScan, snapshot.root == url else { return nil }
    discardCache()
    return snapshot.expires > ProcessInfo.processInfo.systemUptime ? snapshot : nil
  }

  private func cacheCurrentScan() -> Bool {
    guard let root, let core = handle, let monitor, let watchID,
      status.done, status.error == nil, status.treeBytes <= Self.maxCachedTreeBytes,
      cacheEligible, !refreshBusy, dirty.count <= Self.maxCachedChanges
    else { return false }

    discardCache()
    let expires = ProcessInfo.processInfo.systemUptime + Self.cacheLifetime
    cachedScan = CachedScan(
      root: root, core: core, status: status, monitor: monitor, watchID: watchID,
      expires: expires, dirty: dirty)
    let expiry = DispatchWorkItem { [weak self] in
      guard let self, self.cachedScan?.expires == expires else { return }
      self.discardCache()
    }
    cacheExpiry = expiry
    DispatchQueue.main.asyncAfter(deadline: .now() + Self.cacheLifetime, execute: expiry)
    return true
  }

  private func receiveChanges(_ urls: [URL], lost: Bool, from id: UInt32) {
    if watchID == id {
      dirty.formUnion(urls)
      if lost || dirty.count > 4096 {
        // A stream gap disqualifies this snapshot even if a file operation
        // is currently preventing an immediate rescan.
        watchID = nil
        monitor?.stop()
        monitor = nil
        if operation {
          notice = tr("Changes were missed. Rescan to update this view.", "部分文件变更已丢失，请重新扫描。")
        } else {
          rescan()
          notice = tr("Changes were missed. Rescanning this location.", "部分文件变更已丢失，正在重新扫描。")
        }
      } else {
        scheduleDirty()
      }
    } else if cachedScan?.watchID == id {
      cachedScan?.dirty.formUnion(urls)
      if lost || (cachedScan?.dirty.count ?? 0) > Self.maxCachedChanges {
        discardCache()
      }
    }
  }

  func start(_ url: URL, force: Bool = false) {
    guard !operation else { return }
    if requireAccessSetup && !preferences.bool(forKey: Self.accessSetupKey) {
      pendingScan = url
      accessSetupPresented = true
      return
    }
    let location = url.standardizedFileURL.resolvingSymlinksInPath()
    guard force || location != root else { return }
    let restored = force ? nil : takeCache(for: location)
    if force && cachedScan?.root == location { discardCache() }

    timer?.invalidate()
    timer = nil
    dirtyWork?.cancel()
    dirtyWork = nil
    if force || !cacheCurrentScan() {
      monitor?.stop()
      handle?.cancel()
    }
    monitor = nil
    handle = nil
    watchID = nil
    pollBusy = false
    refreshBusy = false
    cacheEligible = true
    generation &+= 1
    let gen = generation
    root = location
    view = 0
    selection = []
    selected = nil
    entries = []
    categories = []
    categoryFilter = nil
    crumbs = []
    up = nil
    history = []
    forward = []
    undoBatches = []
    status = restored?.status ?? ScanStatus()
    dirty = restored?.dirty ?? []
    notice = nil
    lastProgress = status.scanned
    scanFrames = 0

    if let restored {
      handle = restored.core
      monitor = restored.monitor
      watchID = restored.watchID
      changed()
      // The retained watcher recorded changes while this location was away.
      // Reconcile those directories without replacing the scan.
      flushDirty()
      poll(restored.core, generation: gen)
      return
    }

    watchID = gen
    // FSEvents opens the root's parent directories while installing the
    // watcher. The system can block those opens on privacy checks.
    DispatchQueue.global(qos: .userInitiated).async { [weak self] in
      let watcher = Result {
        try ChangeMonitor(url: location) { [weak self] urls, lost in
          self?.receiveChanges(urls, lost: lost, from: gen)
        }
      }
      DispatchQueue.main.async {
        guard let self, self.generation == gen else { return }
        switch watcher {
        case .success(let monitor):
          self.monitor = monitor
        case .failure(let error):
          self.watchID = nil
          self.notice = tr("Live updates unavailable: ", "实时更新不可用：") + error.localizedDescription
        }
        let core = CoreHandle(url: location)
        self.handle = core
        self.timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
          Task { @MainActor in self?.poll(core, generation: gen) }
        }
        self.changed()
      }
    }
    changed()
  }
  func poll(_ core: CoreHandle, generation gen: UInt32) {
    guard generation == gen, !pollBusy else { return }
    pollBusy = true
    worker.async { [weak self] in
      let result = try? decode(ScanStatus.self, ts_status(core.pointer))
      DispatchQueue.main.async {
        guard let self, self.generation == gen else { return }
        self.pollBusy = false
        guard let result else { return }
        self.status = result
        if let error = result.error { self.notice = reasonText(error) }
        if result.scanned != self.lastProgress || result.done {
          self.lastProgress = result.scanned
          self.scanFrames &+= 1
          self.changed()
        }
        if result.done {
          self.timer?.invalidate()
          self.timer = nil
          self.refreshNav()
          self.loadList(reset: true)
          self.refreshSelection()
          self.scheduleDirty()
        }
      }
    }
  }
  func stop() {
    if let handle {
      handle.cancel()
    } else if isScanning {
      generation &+= 1
      watchID = nil
      status.done = true
      status.error = "cancelled"
      notice = reasonText("cancelled")
      changed()
    }
  }
  func rescan() { if let root { start(root, force: true) } }
  func close() {
    generation &+= 1
    discardCache()
    timer?.invalidate()
    timer = nil
    dirtyWork?.cancel()
    dirtyWork = nil
    monitor?.stop()
    monitor = nil
    watchID = nil
    handle?.cancel()
    handle = nil
    root = nil
    webReady = false
  }
  func changed() {
    render?()
    NotificationCenter.default.post(name: .tesseraState, object: self)
  }
  func setLogical(_ value: Bool) {
    logical = value
    changed()
    loadList(reset: true)
    refreshNav()
  }
  func navigate(_ id: UInt32, record: Bool = true) {
    guard let core = handle, id != view else { return }
    let gen = generation
    worker.async { [weak self] in
      let target = try? decode(Entry.self, ts_details(core.pointer, id))
      DispatchQueue.main.async {
        guard let self, self.generation == gen, target?.dir == true else { return }
        if record {
          self.history.append(self.view)
          self.forward = []
        }
        self.view = id
        self.selection = []
        self.selected = nil
        self.changed()
        self.refreshNav()
        self.loadList(reset: true)
      }
    }
  }
  func goBack() {
    guard let id = history.popLast() else { return }
    forward.append(view)
    navigate(id, record: false)
  }
  func goForward() {
    guard let id = forward.popLast() else { return }
    history.append(view)
    navigate(id, record: false)
  }
  func select(_ ids: [UInt32]) {
    let next = Set(ids.filter { $0 < 0x8000_0000 })
    guard next != selection else { return }
    selection = next
    if selected?.id != next.min() { selected = nil }
    changed()
    refreshSelection()
  }

  private func refreshSelection() {
    guard let id = selection.min(), let core = handle else { return }
    let gen = generation
    worker.async { [weak self] in
      let result = try? decode(Entry.self, ts_details(core.pointer, id))
      DispatchQueue.main.async {
        guard let self, self.generation == gen, self.selection.min() == id else { return }
        if self.selected != result { self.selected = result }
      }
    }
  }
  func refreshNav() {
    guard let core = handle else { return }
    let gen = generation
    let id = view
    let logical = logical
    worker.async { [weak self] in
      let nav = try? decode(Nav.self, ts_nav(core.pointer, id, logical))
      let categories =
        (try? decode([CategoryTotal].self, ts_categories(core.pointer, id, logical))) ?? []
      DispatchQueue.main.async {
        guard let self, self.generation == gen, self.view == id, let nav else { return }
        self.up = nav.up
        self.crumbs = nav.crumbs
        self.categories = categories
      }
    }
  }
  func loadList(reset: Bool) {
    guard listVisible, let core = handle else { return }
    let gen = generation
    let id = view
    let logical = logical
    let offset = reset ? 0 : entries.count
    worker.async { [weak self] in
      let result = try? decode([Entry].self, ts_children(core.pointer, id, UInt32(offset), logical))
      DispatchQueue.main.async {
        guard let self, self.generation == gen, self.view == id, let result else { return }
        if reset { self.entries = result } else { self.entries += result }
      }
    }
  }
  func scheduleDirty() {
    guard !isScanning, !refreshBusy, dirtyWork == nil, !dirty.isEmpty else { return }
    let work = DispatchWorkItem { [weak self] in self?.flushDirty() }
    dirtyWork = work
    DispatchQueue.main.asyncAfter(deadline: .now() + (active ? 0.5 : 2), execute: work)
  }
  func flushDirty() {
    dirtyWork?.cancel()
    dirtyWork = nil
    guard !isScanning, !refreshBusy, !dirty.isEmpty, let core = handle else { return }
    refreshBusy = true
    let urls = dirty
    dirty = []
    let gen = generation
    worker.async { [weak self] in
      var changed = false
      var failure: String?
      for url in urls {
        let path = url.withUnsafeFileSystemRepresentation { p in
          Array(
            UnsafeBufferPointer(
              start: UnsafeRawPointer(p!).assumingMemoryBound(to: UInt8.self), count: strlen(p!)))
        }
        let data = path.withUnsafeBufferPointer {
          unpack(ts_refresh(core.pointer, $0.baseAddress, $0.count))
        }
        if let result = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
          changed = changed || (result["changed"] as? Bool ?? false)
          if let error = result["error"] as? String { failure = error }
        }
      }
      DispatchQueue.main.async {
        guard let self, self.generation == gen else { return }
        self.refreshBusy = false
        if changed { self.poll(core, generation: gen) }
        if let failure {
          self.cacheEligible = false
          self.notice = tr("Some changes could not be read: ", "部分变更暂时无法读取：") + failure
        }
        self.scheduleDirty()
      }
    }
  }
  func setCategory(_ id: Int) {
    categoryFilter = categoryFilter == id ? nil : id
    changed()
  }
  func reveal() {
    if let entry = selected {
      NSWorkspace.shared.activateFileViewerSelecting([entry.url])
      return
    }
    guard let core = handle, let id = selection.first else { return }
    let gen = generation
    worker.async { [weak self] in
      let entry = try? decode(Entry.self, ts_details(core.pointer, id))
      DispatchQueue.main.async {
        guard let self, self.generation == gen, let entry else { return }
        NSWorkspace.shared.activateFileViewerSelecting([entry.url])
      }
    }
  }
  func copyPaths() {
    guard let core = handle else { return }
    let ids = Array(selection)
    worker.async {
      let paths = ids.compactMap { try? decode(Entry.self, ts_details(core.pointer, $0)).path }
      DispatchQueue.main.async {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(paths.joined(separator: "\n"), forType: .string)
      }
    }
  }
  func finishAccessSetup() {
    guard !accessSetupBusy else { return }
    preferences.set(true, forKey: Self.accessSetupKey)
    accessSetupPresented = false
    if let pending = pendingScan {
      pendingScan = nil
      start(pending)
    }
  }

  func selectAccessibleFolders() {
    let panel = NSOpenPanel()
    panel.canChooseDirectories = true
    panel.canChooseFiles = false
    panel.allowsMultipleSelection = true
    panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser
    panel.prompt = tr("Allow Selected Folders", "允许所选文件夹")
    panel.message = tr(
      "Select the folders you want to analyze. You can select Documents, Downloads, Desktop and Pictures together.",
      "选择需要分析的文件夹，可同时选中文稿、下载、桌面和图片。")
    accessSetupBusy = true
    panel.begin { [weak self] response in
      guard let self else { return }
      self.accessSetupBusy = false
      guard response == .OK else { return }
      var bookmarks = self.preferences.array(forKey: Self.folderBookmarksKey) as? [Data] ?? []
      for url in panel.urls where !self.grantedFolders.contains(url) {
        _ = url.startAccessingSecurityScopedResource()
        self.grantedFolders.append(url)
        do {
          bookmarks.append(
            try url.bookmarkData(
              options: .withSecurityScope, includingResourceValuesForKeys: nil, relativeTo: nil))
        } catch {
          self.notice = tr(
            "Folder access is available for this session. Select it again if macOS asks after restarting.",
            "当前会话已允许访问。重启后若系统询问，请重新选择该文件夹。")
        }
      }
      self.preferences.set(bookmarks, forKey: Self.folderBookmarksKey)
      self.grantedFolderCount = self.grantedFolders.count
    }
  }

  private func restoreFolderAccess() {
    for bookmark in preferences.array(forKey: Self.folderBookmarksKey) as? [Data] ?? [] {
      var stale = false
      if let url = try? URL(
        resolvingBookmarkData: bookmark, options: [.withSecurityScope, .withoutUI],
        relativeTo: nil, bookmarkDataIsStale: &stale),
        !stale, !grantedFolders.contains(url)
      {
        _ = url.startAccessingSecurityScopedResource()
        grantedFolders.append(url)
      }
    }
    grantedFolderCount = grantedFolders.count
  }

  func showPermissionHelp() {
    NSWorkspace.shared.open(URL(fileURLWithPath: "/System/Applications/System Settings.app"))
    notice = tr(
      "In Privacy & Security → Full Disk Access, add Tessera for Mac, then scan again. Accessible folders work without this permission.",
      "在“隐私与安全 → 完全磁盘访问权限”中添加 Tessera for Mac，然后重新扫描。无需此权限也能分析可访问的文件夹。")
  }
  func confirm(title: String, body: String, button: String, action: @escaping () -> Void) {
    let alert = NSAlert()
    alert.messageText = title
    alert.informativeText = body
    alert.alertStyle = .warning
    alert.addButton(withTitle: button)
    alert.addButton(withTitle: tr("Cancel", "取消"))
    guard let window = NSApp.keyWindow else { return }
    alert.beginSheetModal(for: window) { response in
      if response == .alertFirstButtonReturn { action() }
    }
  }
  func remove(permanent: Bool) {
    guard canDelete else { return }
    let execute = { [weak self] in self?.performRemove(permanent: permanent) }
    if permanent {
      confirm(
        title: tr("Delete permanently?", "永久删除？"),
        body: tr(
          "These selected items will be deleted immediately. This action cannot be undone.",
          "所选项目将立即删除，此操作无法撤销。"), button: tr("Delete Permanently", "永久删除")
      ) { execute() }
    } else {
      execute()
    }
  }
  private func performRemove(permanent: Bool) {
    guard canDelete, let core = handle else { return }
    operation = true
    changed()
    let ids = Array(selection)
    let gen = generation
    worker.async { [weak self] in
      var failures = [String]()
      var targets = [Entry]()
      var undo = [(original: URL, trash: URL, identity: FileIdentity)]()
      for id in ids {
        let data = unpack(ts_validate(core.pointer, id))
        if let entry = try? JSONDecoder().decode(Entry.self, from: data) {
          targets.append(entry)
        } else {
          let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
          failures.append(reasonText(object?["error"] as? String ?? "staleTarget"))
        }
      }
      targets = targets.filter { candidate in
        !targets.contains { other in
          other.id != candidate.id
            && candidate.url.pathComponents.starts(with: other.url.pathComponents)
        }
      }
      for entry in targets {
        do {
          let data = unpack(ts_validate(core.pointer, entry.id))
          guard let checked = try? JSONDecoder().decode(Entry.self, from: data),
            checked.identity == entry.identity
          else {
            throw NSError(
              domain: "Tessera", code: 1,
              userInfo: [NSLocalizedDescriptionKey: reasonText("staleTarget")])
          }
          if permanent {
            try SecureRemoval.remove(entry)
          } else {
            var result: NSURL?
            try FileManager.default.trashItem(at: entry.url, resultingItemURL: &result)
            if let result, let identity = entry.identity {
              undo.append((entry.url, result as URL, identity))
            }
          }
        } catch { failures.append(entry.name + ": " + error.localizedDescription) }
      }
      DispatchQueue.main.async {
        guard let self, self.generation == gen else { return }
        self.operation = false
        self.selection = []
        self.selected = nil
        if !undo.isEmpty { self.undoBatches.append(undo) }
        self.notice =
          failures.isEmpty
          ? (permanent
            ? tr("Deleted selected items.", "已删除所选项目。")
            : tr("Moved to Trash. Undo is available for this scan session.", "已移入废纸篓，可在当前扫描会话中撤销。"))
          : failures.joined(separator: "\n")
        for entry in targets { self.dirty.insert(entry.url.deletingLastPathComponent()) }
        self.flushDirty()
        self.changed()
      }
    }
  }
  func undo() {
    guard canUndo, let batch = undoBatches.popLast() else { return }
    operation = true
    changed()
    let gen = generation
    worker.async { [weak self] in
      var failures = [String]()
      var remaining = [(original: URL, trash: URL, identity: FileIdentity)]()
      for item in batch {
        do {
          var meta = stat()
          if lstat(item.original.path, &meta) == 0 {
            throw NSError(
              domain: "Tessera", code: 1,
              userInfo: [
                NSLocalizedDescriptionKey: tr(
                  "An item already exists at the original location.", "原位置已有同名项目。")
              ])
          }
          guard lstat(item.trash.path, &meta) == 0, meta.st_ino == item.identity.inode,
            meta.st_dev == item.identity.device
          else {
            throw NSError(
              domain: "Tessera", code: 2,
              userInfo: [NSLocalizedDescriptionKey: reasonText("staleTarget")])
          }
          try FileManager.default.moveItem(at: item.trash, to: item.original)
        } catch {
          failures.append(item.original.lastPathComponent + ": " + error.localizedDescription)
          remaining.append(item)
        }
      }
      DispatchQueue.main.async {
        guard let self, self.generation == gen else { return }
        self.operation = false
        if !remaining.isEmpty { self.undoBatches.append(remaining) }
        self.notice =
          failures.isEmpty
          ? tr("Restored from Trash.", "已从废纸篓恢复。") : failures.joined(separator: "\n")
        for item in batch { self.dirty.insert(item.original.deletingLastPathComponent()) }
        self.flushDirty()
        self.changed()
      }
    }
  }
  func emptyTrash() {
    guard !operation else { return }
    confirm(
      title: tr("Empty the entire Trash?", "清空整个废纸篓？"),
      body: tr(
        "Finder will permanently delete all items in your Trash, including items outside this scan. macOS may ask for permission to control Finder. This cannot be undone.",
        "Finder 将永久删除当前用户整个废纸篓中的项目，包括本次扫描之外的文件。macOS 可能询问控制 Finder 的权限，此操作无法撤销。"),
      button: tr("Empty Trash", "清空废纸篓")
    ) { [weak self] in
      guard let self else { return }
      self.operation = true
      self.changed()
      self.worker.async {
        var error: NSDictionary?
        let script = NSAppleScript(source: "tell application \"Finder\" to empty trash")
        _ = script?.executeAndReturnError(&error)
        let failure = error?[NSAppleScript.errorMessage] as? String
        DispatchQueue.main.async {
          self.operation = false
          if let failure {
            self.notice = failure
            NSWorkspace.shared.open(
              URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent(".Trash"))
          } else {
            self.undoBatches = []
            self.notice = tr("Trash emptied by Finder.", "Finder 已清空废纸篓。")
            self.rescan()
          }
          self.changed()
        }
      }
    }
  }
}
extension Notification.Name { static let tesseraState = Notification.Name("TesseraState") }

/// Descriptor-relative recursive deletion: never traverses symlink targets and
/// rechecks the scanned root identity after opening its parent and directory.
enum SecureRemoval {
  static func fail() throws { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
  static func remove(_ entry: Entry) throws {
    guard let identity = entry.identity else { throw NSError(domain: "Tessera", code: 1) }
    let parent = open(
      entry.url.deletingLastPathComponent().path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
    guard parent >= 0 else {
      try fail()
      return
    }
    defer { close(parent) }
    try unlink(
      parent: parent, name: entry.url.lastPathComponent, device: identity.device, expected: identity
    )
  }
  static func unlink(parent: Int32, name: String, device: Int32, expected: FileIdentity? = nil)
    throws
  {
    var meta = stat()
    guard fstatat(parent, name, &meta, AT_SYMLINK_NOFOLLOW) == 0 else {
      try fail()
      return
    }
    guard meta.st_dev == device else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(EXDEV)) }
    if let expected, meta.st_dev != expected.device || meta.st_ino != expected.inode {
      throw NSError(
        domain: "Tessera", code: 1, userInfo: [NSLocalizedDescriptionKey: reasonText("staleTarget")]
      )
    }
    if meta.st_mode & S_IFMT == S_IFDIR {
      let child = openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
      guard child >= 0 else {
        try fail()
        return
      }
      defer { close(child) }
      var opened = stat()
      guard fstat(child, &opened) == 0, opened.st_ino == meta.st_ino, opened.st_dev == meta.st_dev
      else { throw NSError(domain: "Tessera", code: 1) }
      guard let directory = fdopendir(dup(child)) else {
        try fail()
        return
      }
      defer { closedir(directory) }
      while let item = readdir(directory) {
        let name = withUnsafePointer(to: &item.pointee.d_name) {
          $0.withMemoryRebound(to: CChar.self, capacity: Int(item.pointee.d_namlen) + 1) {
            String(cString: $0)
          }
        }
        if name != "." && name != ".." { try unlink(parent: child, name: name, device: device) }
      }
      guard unlinkat(parent, name, AT_REMOVEDIR) == 0 else {
        try fail()
        return
      }
    } else {
      guard unlinkat(parent, name, 0) == 0 else {
        try fail()
        return
      }
    }
  }
}
