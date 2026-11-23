// Fixture: TSX text holding a decimal character reference above U+FFFF, which
// tree-sitter before 0.26 failed on, losing every definition in the file.

export function SearchIcon(): JSX.Element {
  return <span className="search-icon">&#128269;</span>;
}
