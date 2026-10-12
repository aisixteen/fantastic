// The embedded python texts that had no byte-for-byte pin yet.
//
// Every other bundle readme (web, web_rest, web_ws, file_bridge, static_loader,
// ram_store, scheduler, the bridges, the six AI backends) is already compared to
// python's file. These four were only spot-checked (`contains`), or compared
// only by the parity harness that skips without PYTHON_KERNEL_BIN: the
// exec_bridge readme, the cli readme, the cli help (`print(help.md.rstrip())`)
// and the root readme the kernel seeds. Moving any of them out of a Swift
// literal (into a resource or a shared source) must keep these bytes.

import FantasticCliBundle
import FantasticJSON
import FantasticKernel
import Foundation
import Testing

#if os(macOS)
    import FantasticExecBridge
#endif

private func pythonText(_ relative: String) throws -> String {
    try String(contentsOf: pythonSource(relative), encoding: .utf8)
}

@Suite("Embedded python texts — byte for byte")
struct ReadmeVerbatimPinTests {

    @Test func cliReadmeIsPythonVerbatim() throws {
        #expect(CliBundle().readme == (try pythonText("cli/src/cli/readme.md")))
    }

    /// `fantastic help` prints `help.md.rstrip()` + the newline `print` adds;
    /// the constant holds the rstripped text.
    @Test func cliHelpIsPythonsRstrippedHelpMd() throws {
        let py = try pythonText("cli/src/cli/help.md")
        var stripped = py
        while let last = stripped.unicodeScalars.last,
            CharacterSet.whitespacesAndNewlines.contains(last)
        {
            stripped.unicodeScalars.removeLast()
        }
        #expect(CliBundle.helpText == stripped)
    }

    @Test func rootReadmeIsTheKernelStateReadme() throws {
        #expect(!RootReadme.text.isEmpty)
        #expect(
            RootReadme.text == (try pythonText("loader/kernel_state/src/kernel_state/readme.md")))
    }

    #if os(macOS)
        @Test func execBridgeReadmeIsPythonVerbatim() throws {
            #expect(
                ExecBridgeBundle().readme
                    == (try pythonText("io/exec_bridge/src/exec_bridge/readme.md")))
        }
    #endif
}
