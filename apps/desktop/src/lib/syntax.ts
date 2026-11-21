// Light syntax coloring for the preview pane: comments, strings, numbers and keywords only.
// One linear pass, no grammar, no dependency: good enough to make code readable at a glance,
// and cheap on the 64K characters a preview holds. Unknown languages get strings and numbers.

/** Token classes, styled by `.syn-*` rules in app.css. */
export type SyntaxClass = "com" | "str" | "num" | "kw";

/** A colored span of the text: UTF-16 [start, end), sorted and non-overlapping. */
export interface SyntaxSpan {
  start: number;
  end: number;
  cls: SyntaxClass;
}

interface Lang {
  line?: string[];
  block?: [string, string][];
  quotes: string;
  /** Python-style triple-quoted strings. */
  triple?: boolean;
  keywords?: Set<string>;
  /** Keywords matched in lower case (SQL). */
  caseless?: boolean;
}

const words = (s: string) => new Set(s.split(" "));

const C_LIKE = "if else for while do switch case default break continue return goto sizeof typedef struct union enum const static extern void int char long short float double unsigned signed bool true false null";
const KW: Record<string, string> = {
  rust: "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while Some None Ok Err",
  c: C_LIKE + " include define ifdef ifndef endif inline volatile NULL",
  cpp: C_LIKE + " class namespace template typename public private protected virtual override new delete this using auto nullptr constexpr include define try catch throw",
  csharp: "abstract as async await base bool break case catch class const continue default do double else enum event false finally for foreach if in int interface internal is lock namespace new null object out override private protected public readonly ref return sealed static string struct switch this throw true try using var virtual void while",
  java: "abstract boolean break case catch class const continue default do double else enum extends final finally float for if implements import instanceof int interface long new null package private protected public return static super switch this throw throws true false try void while var",
  kotlin: "as break class continue do else false for fun if in interface is null object package return super this throw true try typealias val var when while import",
  scala: "abstract case catch class def do else extends false final for if implicit import lazy match new null object override package private protected return sealed super this throw trait true try type val var while with yield",
  go: "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var true false nil",
  python: "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield self",
  ruby: "alias and begin break case class def defined do else elsif end ensure false for if in module next nil not or redo rescue retry return self super then true undef unless until when while yield require",
  php: "abstract and array as break case catch class const continue declare default do echo else elseif extends false final for foreach function global if implements include interface namespace new null or private protected public require return static switch this throw true try use var while",
  perl: "my our local sub if elsif else unless while until for foreach return use package last next",
  lua: "and break do else elseif end false for function if in local nil not or repeat return then true until while",
  r: "if else repeat while function for in next break TRUE FALSE NULL NA",
  swift: "class deinit enum extension func import init let protocol struct subscript typealias var break case continue default do else fallthrough for if in return switch where while as false is nil self super true try throw guard",
  dart: "abstract as async await break case catch class const continue default do else enum extends false final finally for if import in is new null return static super switch this throw true try var void while",
  javascript: "async await break case catch class const continue debugger default delete do else export extends false finally for from function if import in instanceof let new null of return super switch this throw true try typeof undefined var void while yield",
  typescript: "abstract any as async await boolean break case catch class const continue declare default delete do else enum export extends false finally for from function if implements import in instanceof interface keyof let never new null number of private protected public readonly return satisfies string super switch this throw true try type typeof undefined unknown var void while yield",
  css: "important inherit initial unset none auto media import supports keyframes",
  sql: "select from where and or not insert into values update set delete create table index view drop alter add join left right inner outer on group by order having limit offset as distinct union all null is in like between case when then else end primary key foreign references default",
  shell: "if then else elif fi for while until do done case esac function return in export local readonly echo exit",
  powershell: "begin break catch class continue data do dynamicparam else elseif end exit filter finally for foreach from function if in param process return switch throw trap try until using while",
  batch: "echo set if else goto call exit for in do not exist defined errorlevel",
  json: "true false null",
  toml: "true false",
  yaml: "true false null yes no",
  haskell: "case class data deriving do else if import in infix instance let module newtype of then type where",
  elixir: "def defp defmodule do end if else unless case cond fn true false nil when import alias use",
  zig: "const var fn pub return if else while for break continue struct enum union error try catch defer true false null undefined comptime",
  nim: "proc func var let const type object if elif else while for in return import true false nil",
  dockerfile: "FROM RUN CMD LABEL EXPOSE ENV ADD COPY ENTRYPOINT VOLUME USER WORKDIR ARG ONBUILD SHELL AS",
  graphql: "query mutation subscription fragment on type input enum interface schema extend scalar union directive true false null",
  protobuf: "syntax package import message enum service rpc returns repeated optional required oneof map option true false",
  hcl: "resource variable output module provider data locals true false null for in if",
};

const SLASH: Pick<Lang, "line" | "block"> = { line: ["//"], block: [["/*", "*/"]] };
const HASH: Pick<Lang, "line"> = { line: ["#"] };

function lang(name: string): Lang {
  const keywords = KW[name] ? words(KW[name]) : undefined;
  switch (name) {
    case "rust": case "c": case "cpp": case "csharp": case "java": case "kotlin": case "scala":
    case "swift": case "dart": case "zig": case "protobuf":
      return { ...SLASH, quotes: name === "rust" ? '"' : "\"'", keywords };
    case "go": case "javascript": case "typescript": case "graphql":
      return { ...SLASH, quotes: "\"'`", keywords, ...(name === "graphql" ? HASH : {}) };
    case "php": case "hcl":
      return { line: ["//", "#"], block: [["/*", "*/"]], quotes: "\"'", keywords };
    case "css":
      return { block: [["/*", "*/"]], quotes: "\"'", keywords };
    case "python":
      return { ...HASH, quotes: "\"'", triple: true, keywords };
    case "ruby": case "perl": case "r": case "shell": case "yaml": case "toml": case "elixir": case "nim":
    case "dockerfile": case "makefile":
      return { ...HASH, quotes: name === "makefile" ? "" : "\"'", keywords };
    case "powershell":
      return { ...HASH, block: [["<#", "#>"]], quotes: "\"'", keywords };
    case "ini":
      return { line: [";", "#"], quotes: "\"", keywords };
    case "sql":
      return { line: ["--"], block: [["/*", "*/"]], quotes: "'\"", keywords, caseless: true };
    case "lua": case "haskell":
      return { line: ["--"], quotes: "\"'", keywords };
    case "batch":
      return { line: ["REM ", "rem ", "::"], quotes: "\"", keywords, caseless: true };
    case "xml":
      return { block: [["<!--", "-->"]], quotes: "\"'" };
    case "json":
      return { quotes: "\"", keywords };
    default:
      return { quotes: "\"'", keywords };
  }
}

const isWordStart = (c: number) =>
  (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95 || c === 36 || c > 127;
const isWordPart = (c: number) => isWordStart(c) || (c >= 48 && c <= 57);
const isDigit = (c: number) => c >= 48 && c <= 57;

/** Colored spans of `text` in `language` (a hint from the backend, e.g. "rust"). */
export function highlightSyntax(text: string, language: string): SyntaxSpan[] {
  const L = lang(language);
  const spans: SyntaxSpan[] = [];
  const n = text.length;
  let i = 0;
  const push = (start: number, end: number, cls: SyntaxClass) => {
    if (end > start) spans.push({ start, end, cls });
  };
  outer: while (i < n) {
    const c = text.charCodeAt(i);
    // Comments
    for (const marker of L.line ?? []) {
      if (text.startsWith(marker, i)) {
        let end = text.indexOf("\n", i);
        if (end < 0) end = n;
        push(i, end, "com");
        i = end;
        continue outer;
      }
    }
    for (const [open, close] of L.block ?? []) {
      if (text.startsWith(open, i)) {
        const at = text.indexOf(close, i + open.length);
        const end = at < 0 ? n : at + close.length;
        push(i, end, "com");
        i = end;
        continue outer;
      }
    }
    // Strings (single line, except triple quotes and template literals)
    const ch = text[i];
    if (L.quotes.includes(ch)) {
      if (L.triple && text.startsWith(ch.repeat(3), i)) {
        const at = text.indexOf(ch.repeat(3), i + 3);
        const end = at < 0 ? n : at + 3;
        push(i, end, "str");
        i = end;
        continue;
      }
      let j = i + 1;
      while (j < n) {
        const d = text[j];
        if (d === "\\") { j += 2; continue; }
        if (d === ch) { j++; break; }
        if (d === "\n" && ch !== "`") break;
        j++;
      }
      push(i, Math.min(j, n), "str");
      i = j;
      continue;
    }
    // Numbers (not inside identifiers)
    if (isDigit(c) && (i === 0 || !isWordPart(text.charCodeAt(i - 1)))) {
      let j = i + 1;
      while (j < n && (isWordPart(text.charCodeAt(j)) || (text[j] === "." && isDigit(text.charCodeAt(j + 1))))) j++;
      push(i, j, "num");
      i = j;
      continue;
    }
    // Identifiers and keywords
    if (isWordStart(c)) {
      let j = i + 1;
      while (j < n && isWordPart(text.charCodeAt(j))) j++;
      if (L.keywords) {
        const w = text.slice(i, j);
        if (L.keywords.has(L.caseless ? w.toLowerCase() : w)) push(i, j, "kw");
      }
      i = j;
      continue;
    }
    // XML tags: color the tag name.
    if (language === "xml" && ch === "<") {
      let j = i + 1;
      if (text[j] === "/" || text[j] === "?" || text[j] === "!") j++;
      const s = j;
      while (j < n && /[\w:.-]/.test(text[j])) j++;
      push(s, j, "kw");
      i = j > i + 1 ? j : i + 1;
      continue;
    }
    i++;
  }
  return spans;
}
