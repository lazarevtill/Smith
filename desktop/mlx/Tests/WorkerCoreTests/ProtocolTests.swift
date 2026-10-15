import Darwin
import Foundation
import XCTest

@testable import WorkerCore

final class ProtocolTests: XCTestCase {
  func testExactIDsAndStrictFields() throws {
    XCTAssertEqual(try decode(["v": 1, "id": "hello-1", "op": "hello"]).id, "hello-1")
    for object: [String: Any] in [
      ["v": 2, "id": "x", "op": "hello"], ["v": 1, "id": "../x", "op": "hello"],
      ["v": 1, "id": "x", "op": "hello", "url": "https://example.test"],
      ["v": 1, "id": "x", "op": "vision"], ["v": 1, "id": "", "op": "hello"],
    ] { XCTAssertThrowsError(try decode(object)) }
  }
  func testDuplicateNestedKeysUnicodeAndJSONDepth() throws {
    for text in [
      #"{"v":1,"v":1,"id":"x","op":"hello"}"#,
      #"{"a":{"x":1,"\u0078":2}}"#,
      #"{"x":"\uD800"}"#,
    ] { XCTAssertThrowsError(try BoundedJSON.object(Data(text.utf8))) }
    XCTAssertThrowsError(
      try BoundedJSON.object(
        Data(
          ("{\"x\":" + String(repeating: "[", count: 40) + "0" + String(repeating: "]", count: 40)
            + "}").utf8)))
    XCTAssertThrowsError(try BoundedJSON.object(Data([123, 34, 120, 34, 58, 34, 255, 34, 125])))
  }
  func testGenerationBoundsAndNoSilentModeFallback() throws {
    var object: [String: Any] = [
      "v": 1, "id": "x", "op": "generate", "mode": "raw", "prompt": "Привет 🌍", "max_tokens": 128,
      "temperature": 0.6, "top_p": 0.9,
    ]
    XCTAssertEqual(try decode(object).prompt, "Привет 🌍")
    object["max_tokens"] = 0
    XCTAssertThrowsError(try decode(object))
    object["max_tokens"] = 2049
    XCTAssertThrowsError(try decode(object))
    object["max_tokens"] = 10
    object["mode"] = "chat"
    XCTAssertThrowsError(try decode(object))
    object.removeValue(forKey: "prompt")
    object["messages"] = [["role": "user", "content": "hello", "url": "no"]]
    XCTAssertThrowsError(try decode(object))
    object["messages"] = [["role": "tool", "content": "hello"]]
    XCTAssertThrowsError(try decode(object))
    object["messages"] = [["role": "user", "content": "hello"]]
    XCTAssertEqual(try decode(object).messages?.count, 1)
  }
  func testFramesBoundedAndEOFIsNotCompletion() throws {
    var fds: [Int32] = [0, 0]
    XCTAssertEqual(pipe(&fds), 0)
    defer {
      close(fds[0])
      close(fds[1])
    }
    let bytes = Data("one\ntwo\npartial".utf8)
    _ = bytes.withUnsafeBytes { write(fds[1], $0.baseAddress, $0.count) }
    close(fds[1])
    fds[1] = -1
    let reader = FrameReader(fd: fds[0])
    XCTAssertEqual(try reader.next(), Data("one".utf8))
    XCTAssertEqual(try reader.next(), Data("two".utf8))
    XCTAssertThrowsError(try reader.next())
    XCTAssertThrowsError(try Request.decode(Data(repeating: 32, count: Limits.frameBytes + 1)))
  }
  func testThinkingModeIsTypedChatOnlyAndExplicitNullCannotBecomeDefault() throws {
    var object: [String: Any] = [
      "v": 1, "id": "thinking", "op": "generate", "mode": "chat",
      "messages": [["role": "user", "content": "literal /no_think"]],
      "max_tokens": 10, "temperature": 0, "top_p": 1,
    ]
    XCTAssertNil(try decode(object).thinking_mode)
    object["thinking_mode"] = "model_default"
    XCTAssertEqual(try decode(object).thinking_mode, .modelDefault)
    object["thinking_mode"] = "disabled"
    XCTAssertEqual(try decode(object).thinking_mode, .disabled)
    for invalid: Any in ["enabled", "auto", "", "DISABLED", NSNull(), false, 0, ["disabled"]] {
      object["thinking_mode"] = invalid
      XCTAssertThrowsError(try decode(object)) {
        XCTAssertEqual(($0 as? WorkerError)?.code, "invalid_request")
      }
    }
    object["mode"] = "raw"
    object["prompt"] = "literal"
    object.removeValue(forKey: "messages")
    for mode in ["model_default", "disabled"] {
      object["thinking_mode"] = mode
      XCTAssertThrowsError(try decode(object)) {
        XCTAssertEqual(($0 as? WorkerError)?.code, "unsupported_thinking_mode")
      }
    }
    XCTAssertThrowsError(try Request.decode(Data(
      #"{"v":1,"id":"x","op":"generate","mode":"chat","messages":[{"role":"user","content":"x"}],"max_tokens":1,"temperature":0,"top_p":1,"thinking_mode":"disabled","thinking_mode":"model_default"}"#.utf8)))
    XCTAssertThrowsError(try decode([
      "v": 1, "id": "hello", "op": "hello", "thinking_mode": "disabled",
    ]))
  }
  func testRevisionPatchesPreserveUnicodeAndWhitespace() throws {
    for (old, new) in [
      ("a ", "a,"), ("🇺", "🇺🇦"), ("e", "e\u{301}"), ("aé", "aè"), ("abcdef", "abc"),
    ] {
      let patch = try TextPatch.difference(previous: old, current: new)
      let joined = Data(old.utf8.prefix(patch.0)) + Data(patch.1.utf8)
      XCTAssertEqual(String(data: joined, encoding: .utf8), new)
    }
  }
  func testTemplatesAreExactAndBounded() throws {
    let template = try XCTUnwrap(SafeTemplate(source: SafeTemplate.chatMLSource))
    XCTAssertEqual(
      try template.render([.init(role: "user", content: "Привет")]),
      "<|im_start|>user\nПривет<|im_end|>\n<|im_start|>assistant\n")
    XCTAssertNil(SafeTemplate(source: "{% for x in range(999999999) %}x{% endfor %}"))
    XCTAssertNil(SafeTemplate(source: SafeTemplate.chatMLSource + " "))
    XCTAssertThrowsError(
      try template.render(Array(repeating: .init(role: "user", content: "x"), count: 65)))
  }
  private func decode(_ object: [String: Any]) throws -> Request {
    try Request.decode(JSONSerialization.data(withJSONObject: object))
  }
}

final class TokenizerPolicyTests: XCTestCase {
  func testPreflightRejectsFatalAndExpandingConfigurations() throws {
    let config: [String: Any] = ["tokenizer_class": "PreTrainedTokenizerFast"]
    let valid: [String: Any] = [
      "model": ["type": "BPE", "vocab": ["a": 0], "merges": []],
      "pre_tokenizer": ["type": "ByteLevel"], "decoder": ["type": "ByteLevel"],
    ]
    XCTAssertNoThrow(
      try TokenizerPolicy.validate(data: valid, config: config, vocabularyLimit: 256))
    for invalid: [String: Any] in [
      ["model": ["type": "BPE", "vocab": ["a": 0]]],
      ["model": ["type": "BPE", "vocab": ["a": 0], "merges": [["only-one"]]]],
      valid.merging(["normalizer": ["type": "Unknown"]]) { _, new in new },
      valid.merging([
        "pre_tokenizer": ["type": "Split", "pattern": ["Regex": "(a+)+$"], "behavior": "Isolated"]
      ]) { _, new in new },
      valid.merging([
        "post_processor": [
          "type": "TemplateProcessing", "single": [["Sequence": ["id": "B"]]], "pair": [],
        ]
      ]) { _, new in new },
    ] {
      XCTAssertThrowsError(
        try TokenizerPolicy.validate(data: invalid, config: config, vocabularyLimit: 256))
    }
  }
  func testLoadRequiresApprovedLowercaseContentHash() throws {
    let root: [String: Any] = ["v": 1, "id": "load", "op": "load", "model_root": "/synthetic"]
    XCTAssertThrowsError(try Request.decode(JSONSerialization.data(withJSONObject: root)))
    for hash in [
      String(repeating: "a", count: 63), String(repeating: "A", count: 64),
      String(repeating: "z", count: 64),
    ] {
      XCTAssertThrowsError(
        try Request.decode(
          JSONSerialization.data(
            withJSONObject: root.merging(["expected_package_sha256": hash]) { _, new in new })))
    }
    XCTAssertNoThrow(
      try Request.decode(
        JSONSerialization.data(
          withJSONObject: root.merging([
            "expected_package_sha256": String(repeating: "a", count: 64)
          ]) { _, new in new })))
  }
}
