// The message log's on-disk form. Twin of python
// `tests/kernel/test_message_persistence.py` (the xml half); rust
// `messages_xml/tests.rs`. BYTE PARITY with python's writer is asserted on a
// fixture python itself produced (2026-09-12, `kernel_state.messages_xml`).

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private func row(_ pairs: KeyValuePairs<String, JSON>) -> Row {
    var r: Row = [:]
    for (k, v) in pairs { r[k] = v }
    return r
}

/// A row as the reader hands it back: `.null` fields are omitted on write and
/// absent on read, and attributes come before children — python compares dicts
/// (order-blind), so the twin compares unordered maps too.
private func withoutNulls(_ r: Row) -> [String: JSON] {
    var out: [String: JSON] = [:]
    for (k, v) in r where v != .null { out[k] = v }
    return out
}

private func unordered(_ rows: [Row]) -> [[String: JSON]] {
    rows.map { r in
        var out: [String: JSON] = [:]
        for (k, v) in r { out[k] = v }
        return out
    }
}

@Suite("messages.xml")
struct MessagesXMLTests {
    static let fixture: [Row] = [
        row([
            "id": .integer(1), "agent": .string("ai"), "role": .string("assistant"), "parent": .null,
            "ts": .double(1.5),
            "content": .string("a </content> & </row> <messages> inside\nline 2\n"),
            "title": .string("quoted \"title\" with <tag>"),
        ]),
        row(["id": .integer(2), "agent": .string("ai"), "kind": .string("call"), "verb": .string("read"), "internal": .bool(true), "name": .string("job\tx")]),
        row(["id": .integer(3), "agent": .string("ai"), "kind": .string("fold"), "level": .integer(2), "refs": .array([.integer(1), .integer(2)])]),
        row(["id": .integer(4), "agent": .string("ai"), "kind": .string("view"), "ids": .array([.object(["id": .integer(3), "res": .string("full")])]), "budget": .integer(12), "why": .string("t")]),
        row(["id": .integer(6), "agent": .string("ai"), "kind": .string("late"), "content": .string("")]),
        row(["id": .integer(7), "agent": .string("ai"), "kind": .string("settle"), "parent": .integer(2), "outcome": .object(["status": .string("completed")]), "ts": .double(1789000000.12)]),
        row(["id": .integer(8), "agent": .string("ai"), "big": .double(1e16), "small": .double(0.00001), "neg": .double(-2.5), "items": .array([.null, .bool(true), .string("x"), .integer(3)])]),
    ]

    /// What python's `row_to_xml` wrote for the SAME rows — the bytes another
    /// runtime will read back, so ours must be identical.
    static let pythonBytes =
        "<row id=\"1\" agent=\"ai\" role=\"assistant\" ts=\"1.5\"><content>a &lt;/content&gt; &amp; &lt;/row&gt; &lt;messages&gt; inside\nline 2\n</content><title>quoted \"title\" with &lt;tag&gt;</title></row>\n"
        + "<row id=\"2\" agent=\"ai\" kind=\"call\" verb=\"read\" internal=\"true\" name=\"job&#09;x\" />\n"
        + "<row id=\"3\" agent=\"ai\" kind=\"fold\" level=\"2\"><refs><item>1</item><item>2</item></refs></row>\n"
        + "<row id=\"4\" agent=\"ai\" kind=\"view\" budget=\"12\"><ids><item id=\"3\" res=\"full\" /></ids><why>t</why></row>\n"
        + "<row id=\"6\" agent=\"ai\" kind=\"late\"><content /></row>\n"
        + "<row id=\"7\" agent=\"ai\" kind=\"settle\" parent=\"2\" ts=\"1789000000.12\"><outcome status=\"completed\" /></row>\n"
        + "<row id=\"8\" agent=\"ai\" big=\"1e+16\" small=\"1e-05\" neg=\"-2.5\"><items><item null=\"true\" /><item>true</item><item>x</item><item>3</item></items></row>\n"

    @Test func theWriterMatchesPythonByteForByte() {
        let ours = Self.fixture.map(MessagesXML.rowToXml).joined()
        #expect(ours == Self.pythonBytes)
        #expect(MessagesXML.header == "<messages v=\"1\">\n")
    }

    @Test func xmlRowsRoundTripEveryShapeAndEscape() throws {
        // The library escapes; a body may hold the log's own tags, `&`,
        // newlines, and a megabyte, and every scalar comes back typed.
        var rows = Self.fixture
        rows.append(row(["id": .integer(5), "agent": .string("ai"), "kind": .string("body"), "parent": .integer(2), "content": .string(String(repeating: "x", count: 1_000_000))]))
        let text = MessagesXML.header + rows.map(MessagesXML.rowToXml).joined()
        let back = try MessagesXML.parseMessages(text)
        #expect(unordered(back) == rows.map(withoutNulls))
    }

    @Test func pythonsBytesReadBackTyped() throws {
        let back = try MessagesXML.parseMessages(MessagesXML.header + Self.pythonBytes)
        #expect(unordered(back) == Self.fixture.map(withoutNulls))
        #expect(back[1]["name"] == .string("job\tx"), "a tab in an attribute survives as &#09;")
        #expect(back[5]["ts"] == .double(1789000000.12))
        #expect(back[6]["items"] == .array([.null, .bool(true), .string("x"), .integer(3)]))
    }

    @Test func aCutLastRowIsALoudErrorNotARepair() {
        let good = MessagesXML.rowToXml(row(["id": .integer(1), "agent": .string("ai"), "content": .string("ok")]))
        let whole = MessagesXML.rowToXml(row(["id": .integer(2), "agent": .string("ai"), "content": .string("lost")]))
        let cut = String(whole.dropLast(12))
        #expect(throws: MessagesXMLError.self) {
            try MessagesXML.parseMessages(MessagesXML.header + good + cut)
        }
        do {
            _ = try MessagesXML.parseMessages(MessagesXML.header + good + cut)
        } catch {
            #expect("\(error)".contains("row 1"), "\(error)")
        }
    }

    @Test func anOpenRootIsTheNormalEnd() throws {
        // EOF without `</messages>` is how every log ends; an empty log is no rows.
        #expect(try MessagesXML.parseMessages(MessagesXML.header).isEmpty)
        #expect(try MessagesXML.parseMessages("").isEmpty)
        let one = try MessagesXML.parseMessages(MessagesXML.header + "<row id=\"1\" agent=\"a\" />\n")
        #expect(one.count == 1 && one[0]["id"] == .integer(1))
    }

    @Test func newlinesNormaliseBeforeEntitiesUnescape() throws {
        // A literal `\r\n` in element text is normalised to `\n` (lossy, like
        // ElementTree); an escaped `&#13;` in an attribute survives.
        let text = MessagesXML.header + "<row id=\"1\" agent=\"a\" tag=\"a&#13;b\"><content>x\r\ny</content></row>\n"
        let back = try MessagesXML.parseMessages(text)
        #expect(back[0]["tag"] == .string("a\rb"))
        #expect(back[0]["content"] == .string("x\ny"))
    }
}
