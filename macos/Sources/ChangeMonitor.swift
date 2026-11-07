import Foundation

/// Uses the same owned FSEvents source as the CLI; coalesces callbacks before
/// crossing to MainActor so a large build cannot flood the UI event queue.
final class ChangeMonitor {
  private final class Box {
    let root: URL
    let handler: @MainActor ([URL], Bool) -> Void
    let lock = NSLock()
    var pending = Set<URL>()
    var lost = false
    var scheduled = false
    var active = true
    init(root: URL, handler: @escaping @MainActor ([URL], Bool) -> Void) {
      self.root = root
      self.handler = handler
    }
    func receive(_ bytes: [UInt8], flags: UInt32) {
      let path = String(decoding: bytes, as: UTF8.self)
      let url = URL(fileURLWithPath: path)
      lock.lock()
      defer { lock.unlock() }
      guard active else { return }
      if flags & (1 | 2 | 4 | 8 | 32 | 128) != 0 { lost = true }
      if pending.count < 4096 {
        if flags & 0x20000 != 0, url.pathComponents.starts(with: root.pathComponents) {
          pending.insert(url)
        }
        let parent = url.deletingLastPathComponent()
        if parent.pathComponents.starts(with: root.pathComponents) { pending.insert(parent) }
      } else {
        lost = true
      }
      if !scheduled {
        scheduled = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in self?.deliver() }
      }
    }
    @MainActor func deliver() {
      lock.lock()
      let paths = Array(pending)
      let lost = lost
      let active = active
      pending = []
      self.lost = false
      scheduled = false
      lock.unlock()
      if active { handler(paths, lost) }
    }
  }
  private var stream: UnsafeMutableRawPointer?
  private var box: Box?
  init(url: URL, handler: @escaping @MainActor ([URL], Bool) -> Void) throws {
    let box = Box(root: url, handler: handler)
    self.box = box
    let path = url.withUnsafeFileSystemRepresentation { p in
      Array(
        UnsafeBufferPointer(
          start: UnsafeRawPointer(p!).assumingMemoryBound(to: UInt8.self), count: strlen(p!)))
    }
    stream = path.withUnsafeBufferPointer { bytes in
      ts_watch_create(
        bytes.baseAddress, bytes.count,
        { path, len, flags, context in
          guard let context, let path else { return }
          let box = Unmanaged<Box>.fromOpaque(context).takeUnretainedValue()
          box.receive(Array(UnsafeBufferPointer(start: path, count: len)), flags: flags)
        }, Unmanaged.passUnretained(box).toOpaque())
    }
    if stream == nil { throw NSError(domain: "Tessera.FSEvents", code: 1) }
  }
  func stop() {
    box?.lock.lock()
    box?.active = false
    box?.lock.unlock()
    if let stream {
      ts_watch_release(stream)
      self.stream = nil
    }
    box = nil
  }
  deinit { stop() }
}
