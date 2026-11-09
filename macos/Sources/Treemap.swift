import SwiftUI
@preconcurrency import WebKit

struct Treemap: NSViewRepresentable {
  @ObservedObject var store: Store
  func makeCoordinator() -> Coordinator { Coordinator(store) }
  func makeNSView(context: Context) -> WKWebView {
    Self.makeWebView(coordinator: context.coordinator, store: store)
  }
  static func makeWebView(coordinator: Coordinator, store: Store, frame: CGRect = .zero)
    -> WKWebView
  {
    let configuration = WKWebViewConfiguration()
    configuration.websiteDataStore = .nonPersistent()
    configuration.setURLSchemeHandler(coordinator, forURLScheme: "tessera")
    configuration.userContentController.add(coordinator, name: "tessera")
    let web = WKWebView(frame: frame, configuration: configuration)
    web.underPageBackgroundColor = .clear
    web.navigationDelegate = coordinator
    web.allowsBackForwardNavigationGestures = false
    coordinator.web = web
    store.render = { [weak coordinator] in coordinator?.update() }
    web.load(URLRequest(url: URL(string: "tessera://app/macos.html")!))
    return web
  }
  func updateNSView(_ web: WKWebView, context: Context) { context.coordinator.update() }
  static func dismantleNSView(_ web: WKWebView, coordinator: Coordinator) {
    web.configuration.userContentController.removeScriptMessageHandler(forName: "tessera")
    coordinator.tasks = [:]
    coordinator.store.render = nil
    coordinator.store.webReady = false
  }

  @MainActor
  final class Coordinator: NSObject, WKURLSchemeHandler, WKScriptMessageHandler,
    WKNavigationDelegate
  {
    let store: Store
    weak var web: WKWebView?
    var tasks = [ObjectIdentifier: WKURLSchemeTask]()
    var lastProps: Data?
    var diagnosticGeneration: UInt32?
    init(_ store: Store) { self.store = store }
    func update() {
      guard store.webReady, let web else { return }
      let dark = web.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
      let accent = NSColor.controlAccentColor.usingColorSpace(.sRGB) ?? .systemBlue
      let hex = String(
        format: "#%02x%02x%02x", Int(accent.redComponent * 255), Int(accent.greenComponent * 255),
        Int(accent.blueComponent * 255))
      var props: [String: Any] = [
        "generation": store.generation, "view": store.view, "logical": store.logical,
        "scanning": store.isScanning, "ready": store.handle != nil,
        "rev": store.scanFrames &+ store.status.rev,
        "scheme": dark ? "dark" : "light", "accent": hex, "selection": store.selection.sorted(),
        "reduced": NSWorkspace.shared.accessibilityDisplayShouldReduceMotion,
        "lang": Locale.preferredLanguages.first?.hasPrefix("zh") == true ? "zh-CN" : "en",
        "active": store.active && !(web.window?.isMiniaturized ?? false),
      ]
      if let filter = store.categoryFilter { props["filter"] = filter }
      let signature = try? JSONSerialization.data(withJSONObject: props, options: .sortedKeys)
      if signature != lastProps {
        lastProps = signature
        web.callAsyncJavaScript(
          "window.tessera?.update(state)", arguments: ["state": props], in: nil, in: .page
        ) { result in
          if case .failure(let error) = result {
            NSLog("Tessera render: %@", error.localizedDescription)
          }
        }
      }
      if store.status.done, diagnosticGeneration != store.generation,
        let i = CommandLine.arguments.firstIndex(of: "--diagnostics"),
        CommandLine.arguments.count > i + 1
      {
        diagnosticGeneration = store.generation
        let generation = store.generation
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
          guard let self, self.store.generation == generation else { return }
          web.callAsyncJavaScript(
            "return window.tessera.measure()", arguments: [:], in: nil, in: .page
          ) { result in
            guard case .success(let metrics) = result, let core = self.store.handle,
              let scan = try? JSONSerialization.jsonObject(with: unpack(ts_status(core.pointer)))
            else { return }
            let result: [String: Any] = [
              "renderer": metrics,
              "scan": scan, "host": "native",
              "generation": generation,
            ]
            if let data = try? JSONSerialization.data(
              withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
            {
              try? data.write(to: URL(fileURLWithPath: CommandLine.arguments[i + 1]))
            }
          }
        }
      }
    }
    func userContentController(
      _ userContentController: WKUserContentController, didReceive message: WKScriptMessage
    ) {
      guard message.frameInfo.isMainFrame, message.frameInfo.request.url?.scheme == "tessera",
        let body = message.body as? [String: Any], let action = body["action"] as? String
      else { return }
      if action == "ready" {
        lastProps = nil
        store.webReady = true
        update()
        return
      }
      guard (body["generation"] as? NSNumber)?.uint32Value == store.generation else { return }
      switch action {
      case "render-status":
        if let stats = body["stats"], let data = try? JSONSerialization.data(withJSONObject: stats)
        {
          store.renderStatus = try? JSONDecoder().decode(RenderStatus.self, from: data)
        }
      case "select":
        store.select((body["ids"] as? [NSNumber] ?? []).prefix(100_000).map(\.uint32Value))
      case "navigate": if let id = (body["id"] as? NSNumber)?.uint32Value { store.navigate(id) }
      case "context": showContextMenu()
      case "error": store.notice = body["message"] as? String
      default: break
      }
    }
    func webView(
      _ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
      decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
    ) { decisionHandler(navigationAction.request.url?.scheme == "tessera" ? .allow : .cancel) }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
      store.webReady = false
      webView.reload()
      store.notice = tr(
        "The map process restarted; the scan data was preserved.", "树图进程已重启，扫描数据已保留。")
    }
    func webView(_ webView: WKWebView, start urlSchemeTask: WKURLSchemeTask) {
      let key = ObjectIdentifier(urlSchemeTask)
      tasks[key] = urlSchemeTask
      guard let url = urlSchemeTask.request.url, url.host == "app" else {
        finish(urlSchemeTask, key: key, data: nil, mime: "text/plain", code: 404)
        return
      }
      if url.path == "/tile-name" {
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func value(_ key: String) -> String { items.first { $0.name == key }?.value ?? "" }
        guard let generation = UInt32(value("generation")), generation == store.generation,
          let core = store.handle, let id = UInt32(value("id")), id < 0x8000_0000
        else {
          finish(urlSchemeTask, key: key, data: nil, mime: "text/plain", code: 404)
          return
        }
        store.worker.async { [weak self] in
          let name = unpack(ts_tile_name(core.pointer, id))
          DispatchQueue.main.async {
            guard let self, let task = self.tasks[key] else { return }
            let valid = self.store.generation == generation && !name.isEmpty
            self.finish(
              task, key: key, data: valid ? name : nil, mime: "text/plain; charset=utf-8",
              code: valid ? 200 : 404)
          }
        }
      } else if url.path == "/layout" {
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func value(_ key: String) -> String { items.first { $0.name == key }?.value ?? "" }
        guard let generation = UInt32(value("generation")), generation == store.generation,
          let core = store.handle, let view = UInt32(value("view")),
          let width = Float(value("width")), let height = Float(value("height")), width.isFinite,
          height.isFinite, width > 0, height > 0, width <= 16384, height <= 16384
        else {
          finish(urlSchemeTask, key: key, data: nil, mime: "text/plain", code: 409)
          return
        }
        let logical = value("logical") == "true"
        let force = value("force") == "true"
        store.worker.async { [weak self] in
          let data = unpack(ts_layout(core.pointer, view, width, height, logical, force))
          DispatchQueue.main.async {
            guard let self, let urlSchemeTask = self.tasks[key] else { return }
            guard self.store.generation == generation else {
              self.finish(urlSchemeTask, key: key, data: nil, mime: "text/plain", code: 409)
              return
            }
            self.finish(
              urlSchemeTask, key: key, data: data, mime: "application/octet-stream",
              code: data.isEmpty ? 204 : 200)
          }
        }
      } else {
        guard !url.pathComponents.contains(".."),
          let root = Bundle.main.resourceURL?.appendingPathComponent("web")
        else {
          finish(urlSchemeTask, key: key, data: nil, mime: "text/plain", code: 404)
          return
        }
        let file = root.appendingPathComponent(String(url.path.dropFirst()))
        guard file.standardizedFileURL.pathComponents.starts(with: root.pathComponents) else {
          finish(urlSchemeTask, key: key, data: nil, mime: "text/plain", code: 404)
          return
        }
        let type =
          file.pathExtension == "js"
          ? "application/javascript" : file.pathExtension == "css" ? "text/css" : "text/html"
        let data = try? Data(contentsOf: file)
        finish(urlSchemeTask, key: key, data: data, mime: type, code: data == nil ? 404 : 200)
      }
    }
    func finish(
      _ task: WKURLSchemeTask, key: ObjectIdentifier, data: Data?, mime: String, code: Int
    ) {
      guard tasks.removeValue(forKey: key) != nil, let url = task.request.url else { return }
      let response = HTTPURLResponse(
        url: url, statusCode: code, httpVersion: "HTTP/1.1",
        headerFields: [
          "Content-Type": mime, "Cache-Control": "no-store",
          "Content-Length": String(data?.count ?? 0),
        ])!
      task.didReceive(response)
      if let data { task.didReceive(data) }
      task.didFinish()
    }
    func webView(_ webView: WKWebView, stop urlSchemeTask: WKURLSchemeTask) {
      tasks.removeValue(forKey: ObjectIdentifier(urlSchemeTask))
    }
    func showContextMenu() {
      let menu = NSMenu()
      func add(_ title: String, _ selector: Selector, _ enabled: Bool) {
        let item = NSMenuItem(title: title, action: selector, keyEquivalent: "")
        item.target = self
        item.isEnabled = enabled
        menu.addItem(item)
      }
      add(tr("Show in Finder", "在 Finder 中显示"), #selector(reveal), !store.selection.isEmpty)
      add(tr("Copy Path", "复制路径"), #selector(copyPath), !store.selection.isEmpty)
      menu.addItem(.separator())
      add(tr("Move to Trash", "移入废纸篓"), #selector(trash), store.canDelete)
      add(tr("Delete Permanently…", "永久删除…"), #selector(purge), store.canDelete)
      menu.autoenablesItems = false
      menu.popUp(positioning: nil, at: NSEvent.mouseLocation, in: nil)
    }
    @objc func reveal() { store.reveal() }
    @objc func copyPath() { store.copyPaths() }
    @objc func trash() { store.remove(permanent: false) }
    @objc func purge() { store.remove(permanent: true) }
  }
}
