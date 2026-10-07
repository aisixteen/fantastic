// FOLDER BUNDLES — `<root>/bundles/<name>/bundle.yaml` (owner, 2026-10-08): code
// that EXTENDS a bundled agent (`base`), loaded from this kernel's own root at boot
// (before the tree loads, so an instance's handler resolves) and by the root's
// `reload_bundle`. The base builds the bundle (`AgentBundle.folderBundle`).
// Python `kernel_state.load_bundle` / `load_bundles`; rust `folders.rs`.

import FantasticJSON
import Foundation
import OrderedCollections

public enum Folders {
    /// The bundles dir under a kernel root.
    public static let bundlesDir = "bundles"
    /// A folder bundle's manifest.
    public static let bundleFile = "bundle.yaml"

    /// A folder name is its instances' handler_module: letters, digits, `_`, `-`,
    /// starting with a letter or `_`. Python `_FOLDER_NAME`.
    public static func isFolderName(_ name: String) -> Bool {
        guard let first = name.unicodeScalars.first,
            (first.isASCII && first.properties.isAlphabetic) || first == "_"
        else { return false }
        return name.unicodeScalars.allSatisfy {
            $0.isASCII
                && ($0.properties.isAlphabetic || ("0"..."9").contains($0) || $0 == "_"
                    || $0 == "-")
        }
    }

    /// Load ONE folder bundle `<root>/bundles/<name>/` into `kernel`'s registry.
    /// → nil, or the error — recorded on its catalog row too, never loaded.
    @discardableResult
    public static func loadBundle(kernel: Kernel, root: URL, name: String) -> String? {
        let row: OrderedDictionary<String, JSON> = [
            "name": .string(name),
            "handler_module": .string(name),
            "folder": .string("\(root.lastPathComponent)/\(bundlesDir)/\(name)"),
        ]
        func refuse(_ why: String) -> String {
            let err = "bundles/\(name): \(why)"
            var r = row
            r["error"] = .string(err)
            kernel.bundles.refuseFolder(name, row: .object(r))
            return err
        }
        guard isFolderName(name) else {
            return refuse(
                "a bundle folder is named with letters, digits, _ and - (the name is its "
                    + "instances' handler_module)")
        }
        if kernel.bundles.clashes(name) {
            return refuse("\(pyRepr(name)) is a built-in bundle — a folder may not shadow it")
        }
        let folder = root.appendingPathComponent(bundlesDir).appendingPathComponent(name)
        guard
            let text = try? String(
                contentsOf: folder.appendingPathComponent(bundleFile), encoding: .utf8)
        else { return refuse("no bundle.yaml") }
        let spec: JSON
        do { spec = try YamlIO.load(text) } catch {
            return refuse("bundle.yaml does not parse: \(error)")
        }
        guard case .object = spec else { return refuse("bundle.yaml is not a mapping") }
        let bases = kernel.bundles.folderBases().joined(separator: ", ")
        let known = bases.isEmpty ? "none" : bases
        guard let base = spec["base"].asString, !base.isEmpty else {
            return refuse("base: required — the bundled agent it extends (here: \(known))")
        }
        guard
            let sentence = spec["sentence"].asString?.trimmingCharacters(
                in: .whitespacesAndNewlines),
            !sentence.isEmpty
        else { return refuse("sentence: required — one line saying what it is") }
        guard let baseBundle = kernel.bundles.byName(base), baseBundle.folderBase else {
            return refuse(
                "base: \(pyRepr(base)) is no bundled agent a folder can extend in this kernel — known: \(known)"
            )
        }
        switch baseBundle.folderBundle(
            name: name, folder: folder.standardizedFileURL.resolvingSymlinksInPath(), spec: spec)
        {
        case .built(let b):
            var r = row
            r["base"] = .string(base)
            r["sentence"] = .string(sentence)
            kernel.bundles.registerFolder(name, b, row: .object(r))
            return nil
        case .refused(let why):
            return refuse(why)
        }
    }

    /// Load every folder under `<root>/bundles/` — the cold read at boot.
    public static func loadBundles(kernel: Kernel, root: URL) {
        let dir = root.appendingPathComponent(bundlesDir)
        guard
            let names = try? FileManager.default.contentsOfDirectory(atPath: dir.path)
        else { return }
        for n in names.sorted() {
            var isDir: ObjCBool = false
            if FileManager.default.fileExists(
                atPath: dir.appendingPathComponent(n).path, isDirectory: &isDir),
                isDir.boolValue
            {
                loadBundle(kernel: kernel, root: root, name: n)
            }
        }
    }
}
