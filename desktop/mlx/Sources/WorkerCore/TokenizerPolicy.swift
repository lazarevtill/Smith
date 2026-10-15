import CoreFoundation
import Foundation

public enum TokenizerPolicy {
  public static func validate(data: [String: Any], config: [String: Any], vocabularyLimit: Int)
    throws
  {
    func fail() -> WorkerError {
      WorkerError(
        "unsupported_tokenizer", "The tokenizer is outside the bounded standard BPE profile.")
    }
    guard config["auto_map"] == nil, let className = config["tokenizer_class"] as? String,
      [
        "PreTrainedTokenizer", "PreTrainedTokenizerFast", "TokenizersBackend", "GPT2Tokenizer",
        "GPT2TokenizerFast", "Qwen2Tokenizer", "Qwen2TokenizerFast", "LlamaTokenizer",
        "LlamaTokenizerFast",
      ].contains(className),
      let model = data["model"] as? [String: Any], model["type"] as? String == "BPE",
      let vocab = model["vocab"] as? [String: Any], !vocab.isEmpty, vocab.count <= vocabularyLimit,
      let merges = model["merges"] as? [Any], merges.count <= 524_288
    else { throw fail() }
    var ids = Set<Int>()
    for (token, value) in vocab {
      guard token.utf8.count <= 4096, let number = value as? NSNumber,
        CFGetTypeID(number) != CFBooleanGetTypeID(), number.doubleValue == Double(number.intValue),
        (0..<vocabularyLimit).contains(number.intValue), ids.insert(number.intValue).inserted
      else { throw fail() }
    }
    for merge in merges {
      let pieces: [String]
      if let pair = merge as? [String] {
        pieces = pair
      } else if let pair = merge as? String {
        pieces = pair.components(separatedBy: " ")
      } else {
        throw fail()
      }
      guard pieces.count == 2, pieces.allSatisfy({ $0.utf8.count <= 4096 }) else { throw fail() }
    }
    if let added = data["added_tokens"], !(added is NSNull) {
      guard let tokens = added as? [[String: Any]], tokens.count <= 4096 else { throw fail() }
      for token in tokens {
        guard let content = token["content"] as? String, !content.isEmpty,
          content.utf8.count <= 4096,
          let id = token["id"] as? Int, (0..<vocabularyLimit).contains(id)
        else { throw fail() }
      }
    }
    func component(_ object: Any?, kind: String, depth: Int = 0) throws {
      guard let object, !(object is NSNull) else { return }
      guard depth <= 2, let config = object as? [String: Any], let type = config["type"] as? String
      else { throw fail() }
      if type == "Sequence" {
        let key =
          kind == "normalizer"
          ? "normalizers"
          : kind == "pre_tokenizer"
            ? "pretokenizers" : kind == "decoder" ? "decoders" : "processors"
        guard let children = config[key] as? [Any], !children.isEmpty, children.count <= 4 else {
          throw fail()
        }
        for child in children { try component(child, kind: kind, depth: depth + 1) }
        return
      }
      switch kind {
      case "normalizer":
        guard ["NFC", "NFD", "NFKC", "NFKD", "Lowercase", "Strip", "StripAccents"].contains(type)
        else { throw fail() }
      case "decoder":
        guard ["ByteLevel", "ByteFallback", "Fuse"].contains(type) else { throw fail() }
      case "pre_tokenizer":
        if ["ByteLevel", "Whitespace", "WhitespaceSplit"].contains(type) { return }
        guard type == "Split", let pattern = config["pattern"] as? [String: String],
          let regex = pattern["Regex"],
          knownRegex.contains(regex),
          ["Isolated", "Removed", "MergedWithPrevious", "MergedWithNext", "Contiguous"].contains(
            config["behavior"] as? String ?? "")
        else { throw fail() }
      case "post_processor":
        if type == "ByteLevel" { return }
        guard type == "TemplateProcessing", let single = config["single"] as? [[String: Any]],
          let pair = config["pair"] as? [[String: Any]], single.count <= 8, pair.count <= 16
        else { throw fail() }
        for item in single {
          if let sequence = item["Sequence"] as? [String: Any] {
            guard sequence["id"] as? String == "A" else { throw fail() }
          } else if let special = item["SpecialToken"] as? [String: Any], special["id"] is String {
          } else {
            throw fail()
          }
        }
      default: throw fail()
      }
    }
    for kind in ["normalizer", "pre_tokenizer", "decoder", "post_processor"] {
      try component(data[kind], kind: kind)
    }
  }
  // Published standard GPT-2 / Llama-3 / Qwen-2 byte-level split patterns only; no arbitrary regular expression execution.
  static let knownRegex: Set<String> = [
    #"'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+"#,
    #"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+"#,
    #"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+"#,
  ]
}
