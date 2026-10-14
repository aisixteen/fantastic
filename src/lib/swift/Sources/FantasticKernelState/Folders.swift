// FOLDER BUNDLES — `<root>/bundles/<name>/bundle.yaml` (owner, 2026-10-08): code
// that EXTENDS a bundled agent (`base`), loaded from this kernel's own root at boot
// (before the tree loads, so an instance's handler resolves) and by the root's
// `reload_bundle`. The base builds the bundle (`AgentBundle.folderBundle`).
// Python `kernel_state.load_bundle` / `load_bundles`; rust `fantastic-core
// folders.rs` — read through the clamped `fs`, so a `bundles/<name>` that links
// outside the running dir is refused like any escape (audit 2026-10-14).

import FantasticFile
import FantasticJSON
import FantasticKernel
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

    /// Load ONE folder bundle `<root>/bundles/<name>/` into `kernel`'s registry —
    /// `root` the loader's CLAMPED root. → nil, or the error — recorded on its
    /// catalog row too, never loaded.
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
        let r = root.path
        let rel = "\(bundlesDir)/\(name)"
        // THE CLAMP: the folder resolves inside the root, or it is no folder of
        // this kernel's — a link out of the running dir is refused, never followed.
        let folder: String
        do { folder = try fsResolve(r, rel, base: r) } catch { return refuse(error.message) }
        let manifest = "\(rel)/\(bundleFile)"
        guard (try? fsIsFile(r, manifest, base: r)) == true else {
            return refuse("no bundle.yaml")
        }
        let text: String
        do { text = try fsReadText(r, manifest, base: r) } catch { return refuse(error.message) }
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
            name: name, folder: URL(fileURLWithPath: folder), spec: spec)
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

    /// Load every folder under `<root>/bundles/` — the cold read at boot. `root`
    /// is clamped to its parent (the running dir) first.
    public static func loadBundles(kernel: Kernel, root: URL) {
        let base = root.deletingLastPathComponent().path
        guard let r = try? fsResolveRoot(root.path, base: base),
            (try? fsIsDir(r, bundlesDir, base: r)) == true,
            let entries = try? fsListDir(r, bundlesDir, base: r)
        else { return }
        let clamped = URL(fileURLWithPath: r)
        for path in entries {
            let n = (path as NSString).lastPathComponent
            // a link escaping the root is still offered — `loadBundle` refuses it
            if (try? fsIsDir(r, "\(bundlesDir)/\(n)", base: r)) != false {
                loadBundle(kernel: kernel, root: clamped, name: n)
            }
        }
    }
}
