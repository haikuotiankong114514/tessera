import AppKit
import Foundation
import WebKit

/// Exercises WebKit's actual binary scheme bridge without displaying a window.
@main struct RenderTests {
  @MainActor static func wait(_ condition: () -> Bool, seconds: Double = 10) throws {
    let deadline = Date().addingTimeInterval(seconds)
    while !condition() {
      if Date() > deadline { throw NSError(domain: "RendererTimeout", code: 1) }
      RunLoop.current.run(until: Date().addingTimeInterval(0.01))
    }
  }
  @MainActor static func main() throws {
    NSApplication.shared.setActivationPolicy(.prohibited)
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(
      "tessera-render-" + UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    for n in 0..<50 {
      try Data(repeating: UInt8(n), count: 4096 * (n + 1)).write(
        to: root.appendingPathComponent("file-\(n).txt"))
    }
    let store = Store()
    let coordinator = Treemap.Coordinator(store)
    let web = Treemap.makeWebView(
      coordinator: coordinator, store: store, frame: CGRect(x: 0, y: 0, width: 800, height: 600))
    store.start(root)
    store.active = true
    do { try wait { store.webReady && store.status.done } } catch {
      print("WebKit not ready: \(String(describing:store.notice)); \(String(describing:web.url))")
      throw error
    }
    let file = try decode([Entry].self, ts_children(store.handle!.pointer, 0, 0, false))[0]
    var hoverName: String?
    web.callAsyncJavaScript(
      "return (await fetch(url)).text()",
      arguments: [
        "url": "tessera://app/tile-name?generation=\(store.generation)&id=\(file.id)"
      ], in: nil, in: .page
    ) { result in
      if case .success(let value) = result { hoverName = value as? String }
    }
    try wait { hoverName != nil }
    assert(hoverName == file.name)
    var stats: [String: Any]?
    var resultError: String?
    RunLoop.current.run(until: Date().addingTimeInterval(0.8))
    web.callAsyncJavaScript("return window.tessera.measure()", arguments: [:], in: nil, in: .page) {
      result in
      switch result {
      case .success(let value): stats = value as? [String: Any]
      case .failure(let error): resultError = error.localizedDescription
      }
    }
    try wait { stats != nil || resultError != nil }
    guard let stats, let requests = stats["requests"] as? Int, requests > 0,
      (stats["errors"] as? [String])?.isEmpty == true
    else {
      print(
        "Renderer diagnostics: \(String(describing:stats)) \(String(describing:resultError)) \(String(describing:store.notice))"
      )
      throw NSError(domain: "TesseraRenderer", code: 1)
    }
    print(
      String(
        data: try JSONSerialization.data(withJSONObject: stats, options: [.sortedKeys]),
        encoding: .utf8)!)
    store.close()
  }
}
