// File categories for icons and badges (design §2.5). Purely presentational: the query
// language's `type:` kinds live in Rust; this only decides the color of a row's icon.

export type FileCategory =
  | "folder" | "doc" | "sheet" | "slides" | "image" | "media" | "code" | "archive" | "other";

const EXTENSIONS: Record<Exclude<FileCategory, "folder" | "other">, string[]> = {
  doc: ["pdf", "doc", "docx", "odt", "rtf", "txt", "md", "epub"],
  sheet: ["xls", "xlsx", "ods", "csv", "tsv"],
  slides: ["ppt", "pptx", "odp", "key"],
  image: ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tif", "tiff", "heic", "ico", "psd"],
  media: ["mp4", "mkv", "mov", "avi", "webm", "mp3", "wav", "flac", "ogg", "m4a", "aac"],
  code: [
    "rs", "ts", "tsx", "js", "jsx", "json", "toml", "yaml", "yml", "py", "c", "h", "cpp", "cs",
    "java", "go", "html", "css", "sh", "ps1", "sql", "xml", "lua", "rb", "php", "kt", "swift",
  ],
  archive: ["zip", "7z", "rar", "tar", "gz", "xz", "bz2", "iso"],
};

const BY_EXT = new Map<string, FileCategory>();
for (const [cat, exts] of Object.entries(EXTENSIONS)) {
  for (const e of exts) BY_EXT.set(e, cat as FileCategory);
}

export function categoryOf(ext: string, isDir: boolean): FileCategory {
  if (isDir) return "folder";
  return BY_EXT.get(ext) ?? "other";
}

/** Pictures the backend can compare (`find_similar`, `similar:`): athrix-imagehash's IMAGE_EXTENSIONS. */
const SIMILAR_EXTS = new Set(["jpg", "jpeg", "jpe", "jfif", "png", "webp", "gif", "bmp", "tif", "tiff"]);

/** Whether «صور مشابهة» / "Similar images" is offered for this result. */
export function canFindSimilar(item: { ext: string; isDir: boolean; size: number | null }): boolean {
  return !item.isDir && !!item.size && SIMILAR_EXTS.has(item.ext);
}
