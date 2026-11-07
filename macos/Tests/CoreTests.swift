import AppKit
import Foundation

@main struct NativeTests {
  @MainActor static func wait(_ condition: () -> Bool, seconds: Double = 10) throws {
    let deadline = Date().addingTimeInterval(seconds)
    while !condition() {
      if Date() > deadline { throw NSError(domain: "TestTimeout", code: 1) }
      RunLoop.current.run(until: Date().addingTimeInterval(0.01))
    }
  }
  @MainActor static func main() throws {
    let fm = FileManager.default
    let root = fm.temporaryDirectory.appendingPathComponent(
      "tessera-native-tests-" + UUID().uuidString, isDirectory: true)
    try fm.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? fm.removeItem(at: root) }
    try Data(repeating: 1, count: 16001).write(to: root.appendingPathComponent("file"))
    let core = CoreHandle(url: root)
    try wait { (try? decode(ScanStatus.self, ts_status(core.pointer)).done) == true }
    let status = try decode(ScanStatus.self, ts_status(core.pointer))
    assert(status.files == 1 && status.logical == 16001)
    let list = try decode([Entry].self, ts_children(core.pointer, 0, 0, false))
    assert(list.count == 1)
    let file = list[0]
    let layout = unpack(ts_layout(core.pointer, 0, 800, 600, false, false))
    assert(layout.count >= 32)
    assert(Array(layout.prefix(4)) == [84, 83, 82, 76])
    assert(
      unpack(ts_layout(core.pointer, 0, 800, 600, false, false)).isEmpty,
      "unchanged picture must not cross the WebKit bridge")
    let validated = try decode(Entry.self, ts_validate(core.pointer, file.id))
    assert(validated.identity == file.identity)
    try Data([2]).write(to: root.appendingPathComponent("replacement"))
    try fm.removeItem(at: file.url)
    try fm.moveItem(at: root.appendingPathComponent("replacement"), to: file.url)
    let rejected =
      try JSONSerialization.jsonObject(with: unpack(ts_validate(core.pointer, file.id)))
      as! [String: Any]
    assert(rejected["error"] as? String == "staleTarget")
    let raw = root.path.utf8.map { $0 }
    _ = raw.withUnsafeBufferPointer { unpack(ts_refresh(core.pointer, $0.baseAddress, $0.count)) }
    let replacement = try decode([Entry].self, ts_children(core.pointer, 0, 0, false))[0]
    try SecureRemoval.remove(replacement)
    assert(!fm.fileExists(atPath: file.url.path))
    let stableElapsed = try decode(ScanStatus.self, ts_status(core.pointer)).elapsedMs
    RunLoop.current.run(until: Date().addingTimeInterval(0.1))
    let elapsedAfter = try decode(ScanStatus.self, ts_status(core.pointer)).elapsedMs
    assert(elapsedAfter == stableElapsed)
    // Removing a directory must not traverse a symlink to its sibling.
    let outside = root.appendingPathComponent("outside.txt")
    try Data([1]).write(to: outside)
    let folder = root.appendingPathComponent("symlink-fixture", isDirectory: true)
    try fm.createDirectory(at: folder, withIntermediateDirectories: false)
    try fm.createSymbolicLink(
      at: folder.appendingPathComponent("link"), withDestinationURL: outside)
    _ = raw.withUnsafeBufferPointer { unpack(ts_refresh(core.pointer, $0.baseAddress, $0.count)) }
    let folderEntry = try decode([Entry].self, ts_children(core.pointer, 0, 0, false)).first {
      $0.name == "symlink-fixture"
    }!
    try SecureRemoval.remove(folderEntry)
    assert(!fm.fileExists(atPath: folder.path) && fm.fileExists(atPath: outside.path))
    try fm.removeItem(at: outside)
    // Real native state transitions, using disposable files only.
    try Data([1, 2, 3]).write(to: root.appendingPathComponent("trash-test"))
    let store = Store()
    store.start(root)
    try wait { store.status.done }
    let own = try decode([Entry].self, ts_children(store.handle!.pointer, 0, 0, false))[0]
    store.select([own.id])
    try wait { store.selected != nil }
    store.remove(permanent: false)
    try wait { !store.operation }
    assert(!fm.fileExists(atPath: own.url.path))
    assert(store.canUndo)
    store.undo()
    try wait { !store.operation }
    assert(fm.fileExists(atPath: own.url.path))
    let touch = TouchBarAnchor.Controller(store)
    touch.install()
    guard let bar = touch.bar else { fatalError("Touch Bar missing") }
    _ = touch.touchBar(bar, makeItemForIdentifier: touch.identifier("undo"))
    _ = touch.touchBar(bar, makeItemForIdentifier: touch.identifier("stop"))
    touch.refresh()
    assert(touch.buttons["stop"]?.isEnabled == false)
    store.close()
    print(
      "Native tests passed: C ABI, binary layout, unchanged-layout suppression, identity replacement, descriptor-relative delete, elapsed time, Trash + Undo, Touch Bar state."
    )
  }
}
