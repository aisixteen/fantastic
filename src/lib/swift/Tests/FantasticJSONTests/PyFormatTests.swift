// Python-format helpers — every pin measured with CPython 3.11.

import Foundation
import OrderedCollections
import Testing

@testable import FantasticJSON

@Suite("python format helpers")
struct PyFormatTests {
    @Test func pyReprMatchesCPython() {
        #expect(pyRepr("it's") == "\"it's\"")
        #expect(pyRepr("a\\b") == "'a\\\\b'")
        #expect(pyRepr("\t\r\n") == "'\\t\\r\\n'")
        #expect(pyRepr("both'\"") == "'both\\'\"'")
        #expect(pyRepr("\u{7f}") == "'\\x7f'")
        #expect(pyRepr("\u{85}") == "'\\x85'")
        #expect(pyRepr("\u{a0}") == "'\\xa0'")
        #expect(pyRepr("\u{200b}") == "'\\u200b'")
        #expect(pyRepr("\u{e0001}") == "'\\U000e0001'")
        #expect(pyRepr("é\u{1F600}") == "'é\u{1F600}'")
        #expect(pyRepr("\u{01}") == "'\\x01'")
        #expect(pyRepr(JSON.null) == "None")
        #expect(pyRepr(JSON.string("nope")) == "'nope'")
    }

    @Test func pyStrMatchesCPython() {
        #expect(pyStr(.null) == "None")
        #expect(pyStr(.bool(true)) == "True")
        #expect(pyStr(.bool(false)) == "False")
        #expect(pyStr(.integer(-3)) == "-3")
        #expect(pyStr(.double(1.0)) == "1.0")
        #expect(pyStr(.double(1e16)) == "1e+16")
        #expect(pyStr(.double(1e-05)) == "1e-05")
        #expect(pyStr(.double(0.0001)) == "0.0001")
        #expect(pyStr(.double(9_999_999_999_999_998.0)) == "9999999999999998.0")
        #expect(pyStr(.double(123_456_789_012_345_678.0)) == "1.2345678901234568e+17")
        #expect(pyStr(.double(1.5e-7)) == "1.5e-07")
        #expect(pyStr(.double(-0.0)) == "-0.0")
        #expect(pyStr(.double(5e-324)) == "5e-324")
        #expect(pyStr(.double(.nan)) == "nan")
        #expect(pyStr(.double(-.infinity)) == "-inf")
        #expect(pyStr(.string("x")) == "x")
        #expect(pyStr([1, "a", nil, true]) == "[1, 'a', None, True]")
        #expect(pyStr(["a": ["b": []]]) == "{'a': {'b': []}}")
    }

    @Test func pyTruthyTable() {
        for falsy: JSON in [
            .null, .bool(false), .integer(0), .double(0), .string(""), .array([]), .object([:]),
        ] {
            #expect(!pyTruthy(falsy), "\(falsy)")
        }
        for truthy: JSON in [
            .bool(true), .integer(-1), .double(.nan), .string(" "), [nil], ["k": nil],
        ] {
            #expect(pyTruthy(truthy), "\(truthy)")
        }
    }

    @Test func pyJsonDumpsMatchesCPython() {
        #expect(pyJsonDumps(["a": 1, "b": "\u{2014}"]) == "{\"a\": 1, \"b\": \"\\u2014\"}")
        #expect(pyJsonDumps(.string("\u{1F600}")) == "\"\\ud83d\\ude00\"")
        #expect(pyJsonDumps(.string("\u{7f}")) == "\"\\u007f\"")
        #expect(pyJsonDumps(.string("\u{7f}"), ensureAscii: false) == "\"\u{7f}\"")
        #expect(
            pyJsonDumps(.string("\u{1F600}\u{01}"), ensureAscii: false) == "\"\u{1F600}\\u0001\"")
        #expect(pyJsonDumps(.double(1e-05)) == "1e-05")
        #expect(pyJsonDumps(["k": nil]) == "{\"k\": null}")
        #expect(pyJsonDumps([1, 2.5, false]) == "[1, 2.5, false]")
        #expect(pyJsonDumps(.double(.infinity)) == "Infinity")
        #expect(
            pyJsonDumps(["a": [1, ["b": []]], "c": [:]], indent: 2)
                == "{\n  \"a\": [\n    1,\n    {\n      \"b\": []\n    }\n  ],\n  \"c\": {}\n}")
    }

    @Test func osErrorStrMatchesCPython() {
        #expect(osErrorStr(2, "/no/such/x") == "[Errno 2] No such file or directory: '/no/such/x'")
        #expect(osErrorStr(3) == "[Errno 3] No such process")
        #expect(osErrorStr(2, "/a", "/b") == "[Errno 2] No such file or directory: '/a' -> '/b'")
    }

    @Test func pyIntOrMatchesCPython() {
        func run(_ v: JSON) -> String {
            do { return String(try pyIntOr(v, 6)) } catch { return error.description }
        }
        #expect(run(.null) == "6")
        #expect(run("7") == "7")
        #expect(run(" 7 ") == "7")
        #expect(run("1_0") == "10")
        #expect(run("+5") == "5")
        #expect(run("7.0") == "ValueError: invalid literal for int() with base 10: '7.0'")
        #expect(run(.double(7.9)) == "7")
        #expect(run(.bool(true)) == "1")
        #expect(run(.bool(false)) == "6")
        #expect(run(.integer(0)) == "6")
        #expect(run([]) == "6")
        #expect(
            run([1])
                == "TypeError: int() argument must be a string, a bytes-like object or a real number, not 'list'"
        )
        #expect(run("\u{663}") == "3")
        #expect(run("1__0") == "ValueError: invalid literal for int() with base 10: '1__0'")
        #expect(run("_1") == "ValueError: invalid literal for int() with base 10: '_1'")
        #expect(run("\u{b2}") == "ValueError: invalid literal for int() with base 10: '²'")
        #expect(run(" -7\u{1c}") == "-7")
    }

    @Test func pyIntMatchesCPython() {
        func run(_ v: JSON) -> String {
            do { return String(try pyInt(v)) } catch { return error.description }
        }
        #expect(
            run(.null)
                == "TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        )
        #expect(run(.bool(false)) == "0")
        #expect(run(.double(-7.9)) == "-7")
        #expect(run("") == "ValueError: invalid literal for int() with base 10: ''")
        #expect(run(.double(.nan)) == "ValueError: cannot convert float NaN to integer")
    }

    @Test func pyTypeNameMatchesCPython() {
        #expect(pyTypeName(.null) == "NoneType")
        #expect(pyTypeName(.bool(true)) == "bool")
        #expect(pyTypeName(.integer(1)) == "int")
        #expect(pyTypeName(.double(1)) == "float")
        #expect(pyTypeName("x") == "str")
        #expect(pyTypeName([]) == "list")
        #expect(pyTypeName([:]) == "dict")
    }

    @Test func pyFloatMatchesCPython() {
        func run(_ v: JSON) -> String {
            do { return pyFloatRepr(try pyFloat(v)) } catch { return error.description }
        }
        #expect(run(.integer(7)) == "7.0")
        #expect(run(.bool(true)) == "1.0")
        #expect(run(.double(0.3)) == "0.3")
        #expect(run("30") == "30.0")
        #expect(run(" 1.5 ") == "1.5")
        #expect(run("1_000.5") == "1000.5")
        #expect(run(".5") == "0.5")
        #expect(run("5.") == "5.0")
        #expect(run("1e3") == "1000.0")
        #expect(run("-INF") == "-inf")
        #expect(run("Infinity") == "inf")
        #expect(run("nan") == "nan")
        #expect(run("\u{663}") == "3.0")
        #expect(run("") == "ValueError: could not convert string to float: ''")
        #expect(run("x") == "ValueError: could not convert string to float: 'x'")
        #expect(run("0x10") == "ValueError: could not convert string to float: '0x10'")
        #expect(run("1_") == "ValueError: could not convert string to float: '1_'")
        #expect(run("1e") == "ValueError: could not convert string to float: '1e'")
        #expect(
            run(.null)
                == "TypeError: float() argument must be a string or a real number, not 'NoneType'")
        #expect(
            run([1])
                == "TypeError: float() argument must be a string or a real number, not 'list'")
    }

    @Test func pyOSErrorNamesTheErrnoSubclass() {
        #expect(pyOSErrorName(EAGAIN) == "BlockingIOError")
        #expect(pyOSErrorName(ENOENT) == "FileNotFoundError")
        #expect(pyOSErrorName(EACCES) == "PermissionError")
        #expect(pyOSErrorName(EIO) == "OSError")
        #expect(
            pyOSError(35).description
                == "BlockingIOError: [Errno 35] Resource temporarily unavailable")
        #expect(
            pyOSError(2, "/x").description
                == "FileNotFoundError: [Errno 2] No such file or directory: '/x'")
    }
}
