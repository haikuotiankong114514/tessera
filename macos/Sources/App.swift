import AppKit
import SwiftUI
import UniformTypeIdentifiers

@main struct TesseraApp: App {
  @StateObject private var store = Store(requireAccessSetup: true)
  @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
  var body: some Scene {
    Window("Tessera for Mac", id: "main") {
      Content(store: store)
        .frame(minWidth: 900, minHeight: 560)
        .background(TouchBarAnchor(store: store).frame(width: 0, height: 0))
        .onAppear {
          delegate.store = store
          NSApp.isAutomaticCustomizeTouchBarMenuItemEnabled = true
          if store.root == nil, let index = CommandLine.arguments.firstIndex(of: "--scan"),
            CommandLine.arguments.count > index + 1
          {
            store.start(URL(fileURLWithPath: CommandLine.arguments[index + 1]))
          }
        }
        .sheet(isPresented: $store.accessSetupPresented) {
          AccessSetup(store: store)
        }
        .onDisappear { store.close() }
    }.defaultSize(width: 1160, height: 730)
      .commands {
        CommandGroup(replacing: .newItem) {
          Button(tr("Scan Folder…", "扫描文件夹…")) { store.choose() }.keyboardShortcut("o")
        }
        CommandGroup(after: .newItem) {
          Button(tr("Rescan", "重新扫描")) { store.rescan() }.keyboardShortcut("r").disabled(
            store.root == nil || store.operation)
          Button(tr("Stop Scan", "停止扫描")) { store.stop() }.disabled(!store.isScanning)
        }
        CommandGroup(replacing: .undoRedo) {
          Button(tr("Undo Move to Trash", "撤销移入废纸篓")) { store.undo() }.keyboardShortcut("z")
            .disabled(!store.canUndo)
        }
        CommandGroup(after: .pasteboard) {
          Button(tr("Copy Paths", "复制路径")) { store.copyPaths() }.keyboardShortcut("c").disabled(
            store.selection.isEmpty)
        }
        CommandMenu(tr("Navigate", "前往")) {
          Button(tr("Back", "返回")) { store.goBack() }.keyboardShortcut("[").disabled(
            store.history.isEmpty)
          Button(tr("Forward", "前进")) { store.goForward() }.keyboardShortcut("]").disabled(
            store.forward.isEmpty)
          Button(tr("Up One Level", "上一级")) { if let up = store.up { store.navigate(up) } }
            .keyboardShortcut(.upArrow).disabled(store.up == nil)
          Divider()
          Button(tr("Show in Finder", "在 Finder 中显示")) { store.reveal() }.keyboardShortcut(
            "r", modifiers: [.command, .shift]
          ).disabled(store.selected == nil)
        }
        CommandMenu(tr("Clean Up", "清理")) {
          Button(tr("Move to Trash", "移入废纸篓")) { store.remove(permanent: false) }.keyboardShortcut(
            .delete
          ).disabled(!store.canDelete)
          Button(tr("Delete Permanently…", "永久删除…")) { store.remove(permanent: true) }
            .keyboardShortcut(.delete, modifiers: [.command, .option]).disabled(!store.canDelete)
          Divider()
          Button(tr("Empty Trash…", "清空废纸篓…")) { store.emptyTrash() }.keyboardShortcut(
            .delete, modifiers: [.command, .shift]
          ).disabled(store.operation)
        }
        CommandGroup(after: .toolbar) {
          Toggle(tr("Show File List", "显示文件列表"), isOn: $store.listVisible).keyboardShortcut(
            "l", modifiers: [.command, .option])
          Toggle(tr("Show Inspector", "显示检查器"), isOn: $store.inspectorVisible).keyboardShortcut("i")
        }
        CommandGroup(replacing: .help) {
          Toggle(tr("Show Rendering Diagnostics", "显示渲染诊断"), isOn: $store.showRenderStatus)
          Button(tr("Disk Access Setup…", "磁盘访问设置…")) { store.accessSetupPresented = true }
          Button(tr("Project on GitHub", "GitHub 项目")) {
            NSWorkspace.shared.open(
              URL(string: "https://github.com/haikuotiankong114514/tessera/tree/macos")!)
          }
        }
      }
    Settings {
      VStack(alignment: .leading, spacing: 16) {
        Text("Tessera for Mac").font(.title2)
        Text(
          tr(
            "Performance-first disk analysis. Files and scan results stay on your Mac.",
            "性能优先的磁盘分析工具。文件与扫描结果保留在本机。"))
        Text(
          tr(
            "Language and appearance follow macOS. Size mode can be changed in the toolbar.",
            "语言和外观跟随 macOS，可在工具栏切换大小计算方式。"))
        Button(tr("Check for Updates…", "检查更新…")) {
          NSWorkspace.shared.open(
            URL(string: "https://github.com/haikuotiankong114514/tessera/releases")!)
        }
      }.padding(24).frame(width: 420)
    }
  }
}
@MainActor final class AppDelegate: NSObject, NSApplicationDelegate {
  weak var store: Store?
  func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
  func application(_ sender: NSApplication, openFiles filenames: [String]) {
    if let path = filenames.first { store?.start(URL(fileURLWithPath: path)) }
    sender.reply(toOpenOrPrint: .success)
  }
  func applicationWillTerminate(_ notification: Notification) { store?.close() }
}
struct Content: View {
  @ObservedObject var store: Store
  var body: some View {
    NavigationSplitView {
      List {
        Section(tr("Locations", "位置")) {
          Button {
            store.choose()
          } label: {
            Label(tr("Choose Folder…", "选择文件夹…"), systemImage: "folder.badge.plus")
          }.buttonStyle(.plain)
          Button {
            store.start(FileManager.default.homeDirectoryForCurrentUser)
          } label: {
            Label(tr("Home", "个人文件夹"), systemImage: "house")
          }.buttonStyle(.plain)
        }
        Section(tr("Disks", "磁盘")) {
          ForEach(store.volumes) { volume in
            Button {
              store.start(volume.url)
            } label: {
              VStack(alignment: .leading, spacing: 5) {
                Label(
                  volume.name,
                  systemImage: volume.url.path == "/" ? "internaldrive" : "externaldrive")
                Text("\(sizeText(UInt64(max(0,volume.free)))) \(tr("available","可用"))").font(
                  .caption
                ).foregroundStyle(.secondary)
              }
            }.buttonStyle(.plain).padding(.vertical, 4)
          }
        }
        if store.root != nil {
          Section(tr("Current Scan", "当前扫描")) {
            Label(
              store.root?.lastPathComponent.isEmpty == false
                ? store.root!.lastPathComponent : tr("Startup Disk", "启动磁盘"),
              systemImage: "chart.pie")
            Text(store.root!.path).font(.caption).foregroundStyle(.secondary).textSelection(
              .enabled)
          }
        }
      }.listStyle(.sidebar).navigationSplitViewColumnWidth(min: 160, ideal: 185, max: 240)
    } detail: {
      VStack(spacing: 0) {
        if store.root == nil {
          VStack(spacing: 18) {
            Image(systemName: "square.grid.3x3.fill").font(.system(size: 58, weight: .light))
              .foregroundStyle(.tint)
            Text(tr("See what fills your Mac.", "看清 Mac 的空间去向。")).font(
              .largeTitle.weight(.semibold))
            Text(
              tr(
                "Scan a disk or folder. Explore its space, then clean up with confidence.",
                "扫描磁盘或文件夹，直观查看占用并整理文件。")
            ).foregroundStyle(.secondary)
            Button(tr("Choose a Folder…", "选择文件夹…")) { store.choose() }.buttonStyle(
              .borderedProminent
            ).controlSize(.large)
            Text(tr("No administrator password required.", "无需管理员密码。")).font(.caption)
              .foregroundStyle(.secondary)
          }.frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
          HStack(spacing: 6) {
            ForEach(Array(store.crumbs.enumerated()), id: \.offset) { index, crumb in
              if index > 0 {
                Image(systemName: "chevron.right").font(.caption2).foregroundStyle(.tertiary)
              }
              Button(
                crumb.id == 0
                  ? (store.root?.lastPathComponent.isEmpty == false
                    ? store.root!.lastPathComponent : "/") : crumb.name
              ) { store.navigate(crumb.id) }.buttonStyle(.plain).lineLimit(1)
            }
            Spacer()
          }.font(.callout).padding(.horizontal, 14).padding(.vertical, 9)
          Divider()
          HSplitView {
            if store.listVisible {
              VStack(spacing: 0) {
                List(selection: Binding(get: { store.selection }, set: { store.select(Array($0)) }))
                {
                  ForEach(store.entries) { entry in
                    HStack {
                      Image(systemName: entry.dir ? "folder" : "doc").foregroundStyle(
                        entry.dir ? Color.accentColor : Color.secondary)
                      Text(entry.name).lineLimit(1)
                      Spacer()
                      Text(sizeText(store.logical ? entry.logical : entry.disk)).font(
                        .caption.monospacedDigit()
                      ).foregroundStyle(.secondary)
                    }.tag(entry.id).onTapGesture(count: 2) {
                      if entry.dir { store.navigate(entry.id) }
                    }
                  }
                  Button(tr("Load More", "加载更多")) { store.loadList(reset: false) }.buttonStyle(
                    .borderless
                  ).padding(6)
                }
              }.frame(minWidth: 210, idealWidth: 260, maxWidth: 380)
            }
            Treemap(store: store).frame(minWidth: 280).accessibilityLabel(
              tr(
                "Disk usage treemap. Use Show File List for accessible navigation.",
                "磁盘占用树图，可打开文件列表进行辅助功能导航。"))
            if store.inspectorVisible {
              Inspector(store: store).frame(minWidth: 210, idealWidth: 240, maxWidth: 300)
            }
          }
          Divider()
          HStack(spacing: 12) {
            if store.isScanning {
              ProgressView().controlSize(.small)
              Text(tr("Scanning", "正在扫描"))
            } else {
              Image(
                systemName: store.status.error == nil
                  ? "checkmark.circle" : "exclamationmark.circle"
              ).foregroundStyle(.secondary)
              Text(store.status.error == nil ? tr("Live", "实时更新") : tr("Partial results", "部分结果"))
            }
            if store.showRenderStatus, let stats = store.renderStatus {
              Text(
                "\(stats.gpu ? "WebGL2 · GPU" : "Canvas 2D") · \(stats.frameMs, specifier: "%.1f") ms · \(stats.rects) tiles"
              )
              .monospacedDigit()
            }
            Text(
              "\(store.status.files.formatted()) \(tr("files","个文件")) · \(store.status.dirs.formatted()) \(tr("folders","个文件夹"))"
            ).monospacedDigit()
            Spacer()
            Text(sizeText(store.status.scanned)).monospacedDigit()
            if store.status.errors > 0 {
              Button("\(store.status.errors) \(tr("unreadable","项无法读取"))") {
                store.showPermissionHelp()
              }.buttonStyle(.borderless)
            }
          }.font(.caption).padding(.horizontal, 14).padding(.vertical, 8).foregroundStyle(
            .secondary)
        }
        if let notice = store.notice {
          Divider()
          HStack(alignment: .top) {
            Image(systemName: "info.circle")
            Text(notice).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
            Button {
              store.notice = nil
            } label: {
              Image(systemName: "xmark")
            }.buttonStyle(.plain)
          }.font(.caption).padding(10).background(.quaternary.opacity(0.4))
        }
      }.navigationTitle("Tessera for Mac")
    }
    .toolbar {
      ToolbarItemGroup(placement: .navigation) {
        Button {
          store.goBack()
        } label: {
          Image(systemName: "chevron.left")
        }.help(tr("Back", "返回")).disabled(store.history.isEmpty)
        Button {
          if let up = store.up { store.navigate(up) }
        } label: {
          Image(systemName: "arrow.up")
        }.help(tr("Up One Level", "上一级")).disabled(store.up == nil)
      }
      ToolbarItemGroup {
        Button {
          store.choose()
        } label: {
          Label(tr("Scan Folder", "扫描文件夹"), systemImage: "folder.badge.plus")
        }
        Button {
          store.isScanning ? store.stop() : store.rescan()
        } label: {
          Label(
            store.isScanning ? tr("Stop", "停止") : tr("Rescan", "重新扫描"),
            systemImage: store.isScanning ? "stop.circle" : "arrow.clockwise")
        }.disabled(store.root == nil || store.operation)
      }
      ToolbarItem {
        Picker(
          tr("Size", "大小"),
          selection: Binding(get: { store.logical }, set: { store.setLogical($0) })
        ) {
          Text(tr("Allocated", "已分配空间")).tag(false)
          Text(tr("Logical", "逻辑大小")).tag(true)
        }.pickerStyle(.segmented).frame(width: 190).disabled(store.root == nil)
      }
      ToolbarItemGroup {
        Button {
          store.listVisible.toggle()
          store.loadList(reset: true)
        } label: {
          Image(systemName: "list.bullet")
        }.help(tr("Toggle File List", "切换文件列表"))
        Button {
          store.inspectorVisible.toggle()
        } label: {
          Image(systemName: "sidebar.right")
        }.help(tr("Toggle Inspector", "切换检查器"))
      }
    }
    .onChange(of: store.listVisible) { _, _ in store.loadList(reset: true) }
    .onDrop(of: [UTType.fileURL], isTargeted: nil) { providers in
      guard let provider = providers.first else { return false }
      _ = provider.loadObject(ofClass: URL.self) { url, _ in
        guard let url else { return }
        var directory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: url.path, isDirectory: &directory),
          directory.boolValue
        else { return }
        DispatchQueue.main.async { store.start(url) }
      }
      return true
    }
  }
}
struct Inspector: View {
  @ObservedObject var store: Store
  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 16) {
        if let entry = store.selected {
          Image(systemName: entry.dir ? "folder.fill" : "doc.fill").font(.system(size: 34))
            .foregroundStyle(.tint)
          Text(
            store.selection.count > 1
              ? "\(store.selection.count) \(tr("items selected","项已选择"))" : entry.name
          ).font(.headline).textSelection(.enabled)
          Text(entry.path).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
          Divider()
          row(tr("Allocated", "已分配空间"), sizeText(entry.disk))
          row(tr("Logical", "逻辑大小"), sizeText(entry.logical))
          if entry.dir {
            row(tr("Files", "文件"), entry.files.formatted())
            row(tr("Folders", "文件夹"), entry.dirs.formatted())
          }
          if let modified = entry.modified {
            row(
              tr("Modified", "修改时间"),
              Date(timeIntervalSince1970: Double(modified) / 1000).formatted(
                date: .abbreviated, time: .shortened))
          }
          if entry.flags & 32 != 0 {
            Label(tr("Cloud placeholder", "云端占位文件"), systemImage: "icloud").font(.caption)
          }
          if entry.flags & 8 != 0 {
            Label(tr("Hard link, counted elsewhere", "硬链接，已在其他位置计入"), systemImage: "link").font(
              .caption)
          }
          if let lock = entry.lockReason {
            Text(reasonText(lock)).font(.caption).foregroundStyle(.secondary)
          }
          Button(tr("Show in Finder", "在 Finder 中显示")) { store.reveal() }
          Button(tr("Move to Trash", "移入废纸篓")) { store.remove(permanent: false) }.disabled(
            !store.canDelete || entry.lockReason != nil)
        } else {
          Text(tr("Disk Overview", "磁盘概览")).font(.headline)
          if store.root != nil {
            row(tr("Scanned files", "已统计文件"), sizeText(store.status.scanned))
            row(tr("Volume capacity", "卷容量"), sizeText(store.status.capacity))
            row(tr("Volume available", "卷可用空间"), sizeText(store.status.free))
            Divider()
            Text(
              tr(
                "APFS clones, shared space, snapshots and unreadable folders can make file totals differ from volume usage. Moving items to Trash does not reclaim their space immediately.",
                "APFS 克隆、共享空间、快照与无权限目录会使文件统计不同于卷占用。移入废纸篓不会立即释放对应空间。")
            ).font(.caption).foregroundStyle(.secondary)
          }
          if !store.categories.isEmpty {
            Divider()
            Text(tr("File Types", "文件类型")).font(.headline)
            ForEach(store.categories) { category in
              Button {
                store.setCategory(category.id)
              } label: {
                HStack {
                  Image(
                    systemName: store.categoryFilter == category.id
                      ? "line.3.horizontal.decrease.circle.fill" : "circle"
                  ).foregroundStyle(.tint)
                  Text(category.label)
                  Spacer()
                  Text(sizeText(category.bytes)).monospacedDigit().foregroundStyle(.secondary)
                }
              }.buttonStyle(.plain).font(.caption)
            }
          }
          Text(tr("Select a tile to inspect its size and location.", "选择图块可查看大小和文件位置。")).font(
            .callout
          ).foregroundStyle(.secondary)
        }
      }.padding(16).frame(maxWidth: .infinity, alignment: .leading)
    }.background(.background)
  }
  func row(_ title: String, _ value: String) -> some View {
    VStack(alignment: .leading, spacing: 3) {
      Text(title).font(.caption).foregroundStyle(.secondary)
      Text(value).font(.callout.monospacedDigit()).textSelection(.enabled)
    }
  }
}

struct AccessSetup: View {
  @ObservedObject var store: Store

  var body: some View {
    VStack(alignment: .leading, spacing: 18) {
      Label(tr("Set Up Disk Access", "设置磁盘访问权限"), systemImage: "externaldrive.badge.checkmark")
        .font(.title2)
      Text(
        tr(
          "Choose your access scope before the first scan. macOS controls access to Documents, Downloads, Pictures and other apps’ data.",
          "首次扫描前请设置访问范围。macOS 会保护文稿、下载、图片和其他应用的数据。"))
      GroupBox {
        VStack(alignment: .leading, spacing: 10) {
          Text(tr("For a whole-disk scan", "扫描整个磁盘")).font(.headline)
          Text(
            tr(
              "Open System Settings → Privacy & Security → Full Disk Access, then add and enable Tessera for Mac. macOS may require you to reopen Tessera.",
              "打开系统设置 → 隐私与安全 → 完全磁盘访问权限，添加并启用 Tessera for Mac。系统可能要求重新打开应用。"))
          Button(tr("Open System Settings", "打开系统设置")) { store.showPermissionHelp() }
        }.frame(maxWidth: .infinity, alignment: .leading).padding(6)
      }
      GroupBox {
        VStack(alignment: .leading, spacing: 10) {
          Text(tr("For selected folders", "仅扫描所选文件夹")).font(.headline)
          Text(
            tr(
              "Select the folders you want to analyze in one chooser. Access to additional protected data may still require macOS approval.",
              "可以在一个选择器中批量选择要分析的文件夹。额外的受保护数据仍可能需要系统批准。"))
          Button(tr("Choose Folders…", "选择文件夹…")) { store.selectAccessibleFolders() }
            .disabled(store.accessSetupBusy)
          if store.grantedFolderCount > 0 {
            Text(
              tr(
                "\(store.grantedFolderCount) folders selected",
                "已选择 \(store.grantedFolderCount) 个文件夹")
            ).font(.caption).foregroundStyle(.secondary)
          }
        }.frame(maxWidth: .infinity, alignment: .leading).padding(6)
      }
      HStack {
        Spacer()
        Button(tr("Continue", "继续")) { store.finishAccessSetup() }
          .buttonStyle(.borderedProminent).disabled(store.accessSetupBusy)
      }
    }
    .padding(24)
    .frame(width: 540)
    .interactiveDismissDisabled()
  }
}
