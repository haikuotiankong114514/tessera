import AppKit
import SwiftUI

struct TouchBarAnchor: NSViewRepresentable {
  @ObservedObject var store: Store
  func makeCoordinator() -> Controller { Controller(store) }
  func makeNSView(context: Context) -> Anchor {
    let view = Anchor()
    view.controller = context.coordinator
    return view
  }
  func updateNSView(_ view: Anchor, context: Context) { context.coordinator.refresh() }
  final class Anchor: NSView {
    var controller: Controller?
    override func viewDidMoveToWindow() {
      super.viewDidMoveToWindow()
      if let window {
        controller?.window = window
        controller?.install()
      }
    }
  }
  @MainActor final class Controller: NSObject, NSTouchBarDelegate {
    let store: Store
    weak var window: NSWindow?
    var bar: NSTouchBar?
    var buttons = [String: NSButton]()
    var observer: NSObjectProtocol?
    var phase = ""
    var lastProgressUpdate = Date.distantPast
    var progressLabel: NSTextField?
    func currentPhase() -> String {
      store.root == nil
        ? "pick" : store.isScanning ? "scan" : store.selection.isEmpty ? "browse" : "selection"
    }
    let prefix = "io.github.haikuotiankong114514.tessera.macos."
    init(_ store: Store) {
      self.store = store
      super.init()
      observer = NotificationCenter.default.addObserver(
        forName: .tesseraState, object: store, queue: .main
      ) { [weak self] _ in Task { @MainActor in self?.refresh() } }
    }
    func identifier(_ name: String) -> NSTouchBarItem.Identifier {
      NSTouchBarItem.Identifier(prefix + name)
    }
    func install() {
      let bar = NSTouchBar()
      bar.delegate = self
      phase = currentPhase()
      buttons.removeAll()
      progressLabel = nil
      bar.customizationIdentifier = NSTouchBar.CustomizationIdentifier(prefix + phase)
      let defaults: [String]
      switch phase {
      case "pick": defaults = ["open", "scanDisk"]
      case "scan": defaults = ["progress", "stop"]
      case "selection": defaults = ["reveal", "trash", "undo", "size"]
      default: defaults = ["back", "up", "size", "inspector", "undo"]
      }
      bar.customizationAllowedItemIdentifiers = [
        "open", "scanDisk", "back", "up", "size", "inspector", "reveal", "trash", "undo", "stop",
        "progress",
      ].map(identifier)
      bar.defaultItemIdentifiers = defaults.map(identifier)
      self.bar = bar
      window?.touchBar = bar
      refresh()
    }
    func touchBar(_ touchBar: NSTouchBar, makeItemForIdentifier id: NSTouchBarItem.Identifier)
      -> NSTouchBarItem?
    {
      let name = String(id.rawValue.dropFirst(prefix.count))
      let item = NSCustomTouchBarItem(identifier: id)
      if name == "progress" {
        let field = NSTextField(labelWithString: tr("Scanning…", "正在扫描…"))
        item.view = field
        progressLabel = field
        item.customizationLabel = tr("Scan Progress", "扫描进度")
        return item
      }
      let title: String
      let selector: Selector
      switch name {
      case "open":
        title = tr("Choose Folder", "选择文件夹")
        selector = #selector(openFolder)
      case "scanDisk":
        title = tr("Scan Startup Disk", "扫描启动磁盘")
        selector = #selector(scanDisk)
      case "back":
        title = "←"
        selector = #selector(back)
      case "up":
        title = "↑"
        selector = #selector(up)
      case "size":
        title = tr("Size", "大小")
        selector = #selector(size)
      case "inspector":
        title = tr("Info", "详情")
        selector = #selector(inspector)
      case "reveal":
        title = tr("Finder", "Finder")
        selector = #selector(reveal)
      case "trash":
        title = tr("Trash…", "废纸篓…")
        selector = #selector(trash)
      case "undo":
        title = tr("Undo", "撤销")
        selector = #selector(undo)
      case "stop":
        title = tr("Stop", "停止")
        selector = #selector(stop)
      default: return nil
      }
      let button = NSButton(title: title, target: self, action: selector)
      item.view = button
      item.customizationLabel = title
      buttons[name] = button
      refresh()
      return item
    }
    func refresh() {
      if currentPhase() != phase {
        install()
        return
      }
      buttons["scanDisk"]?.isEnabled = !store.operation
      buttons["open"]?.isEnabled = !store.operation
      buttons["back"]?.isEnabled = !store.history.isEmpty
      buttons["up"]?.isEnabled = store.up != nil
      buttons["reveal"]?.isEnabled = store.selected != nil
      buttons["trash"]?.isEnabled = store.canDelete
      buttons["undo"]?.isEnabled = store.canUndo
      buttons["stop"]?.isEnabled = store.isScanning
      if Date().timeIntervalSince(lastProgressUpdate) >= 0.5 {
        lastProgressUpdate = Date()
        progressLabel?.stringValue = "\(tr("Scanning", "正在扫描")) · \(store.status.files.formatted())"
      }
      buttons["size"]?.title = store.logical ? tr("Logical", "逻辑大小") : tr("Allocated", "已分配空间")
    }
    @objc func scanDisk() { store.start(URL(fileURLWithPath: "/", isDirectory: true)) }
    @objc func openFolder() { store.choose() }
    @objc func back() { store.goBack() }
    @objc func up() { if let up = store.up { store.navigate(up) } }
    @objc func size() { store.setLogical(!store.logical) }
    @objc func inspector() { store.inspectorVisible.toggle() }
    @objc func reveal() { store.reveal() }
    @objc func undo() { store.undo() }
    @objc func stop() { store.stop() }
    @objc func trash() {
      store.confirm(
        title: tr("Move selected items to Trash?", "将所选项目移入废纸篓？"),
        body: tr("You can undo this during the current scan session.", "可在当前扫描会话中撤销。"),
        button: tr("Move to Trash", "移入废纸篓")
      ) { self.store.remove(permanent: false) }
    }
  }
}
