import Claims.Design

/-!
# Fixture table for path claims

Instantiates the design model with real globs. Overlap here is the meaning
SPEC.md R6 gives it, checked by brute force: some path, up to `maxLen`
characters over `a`, `b` and `/`, matches both patterns. The Rust
implementation decides overlap with a product automaton instead, so the
table checks it against this independent definition.
-/

namespace Claims.Fixtures

/-- Glob tokens, as `crates/rosen-core/src/claim.rs` compiles them. -/
inductive Tok where
  | lit (c : Char)
  | one
  | star
  /-- A `**` segment with its `/`: nothing, or any run ending in `/`. -/
  | dirs
  | dirsIn
  /-- A final `**` segment: anything. -/
  | rest

def segmentTokens (segment : String) : List Tok :=
  segment.toList.map fun
    | '?' => .one
    | '*' => .star
    | c => .lit c

def tokensOf : List String → List Tok
  | [] => []
  | ["**"] => [.rest]
  | "**" :: more => .dirs :: tokensOf more
  | [segment] => segmentTokens segment
  | segment :: more => segmentTokens segment ++ [.lit '/'] ++ tokensOf more

/-- Matches a path. Each call consumes a character or a token, so `fuel` of
both lengths plus one is always enough. -/
def matchFuel : Nat → List Tok → List Char → Bool
  | 0, _, _ => false
  | _ + 1, [], s => s.isEmpty
  | _ + 1, .rest :: _, _ => true
  | n + 1, .lit c :: ps, x :: xs => x == c && matchFuel n ps xs
  | n + 1, .one :: ps, x :: xs => x != '/' && matchFuel n ps xs
  | n + 1, .star :: ps, s =>
    matchFuel n ps s ||
      match s with
      | x :: xs => x != '/' && matchFuel n (.star :: ps) xs
      | [] => false
  | n + 1, .dirs :: ps, s =>
    matchFuel n ps s ||
      match s with
      | _ :: xs => matchFuel n (.dirsIn :: ps) xs
      | [] => false
  | n + 1, .dirsIn :: ps, x :: xs => (x == '/' && matchFuel n ps xs) || matchFuel n (.dirsIn :: ps) xs
  | _ + 1, _, [] => false

def globMatches (pattern path : String) : Bool :=
  let tokens := tokensOf (pattern.splitOn "/")
  matchFuel (path.length + tokens.length + 1) tokens path.toList

def maxLen : Nat := 6

/-- Every string over `a`, `b` and `/` of at most `maxLen` characters. -/
def allPaths : List String :=
  (List.range (maxLen + 1)).flatMap fun len =>
    (List.range len).foldl (fun acc _ => acc.flatMap fun s => ['a', 'b', '/'].map s.push) [""]

def overlap (p q : String) : Bool :=
  allPaths.any fun path => globMatches p path && globMatches q path

def patterns : List String :=
  ["a", "b", "a/b", "a/a", "*", "?", "a/*", "*/b", "a/**", "**/b", "**", "a*", "*a",
    "a/?", "b/**", "**/a/**"]

def bit (b : Bool) : String := if b then "1" else "0"

/-- Change 1 holds `held`; change 2 asks for `requested`. -/
def fixtureRow (held requested : String) : String :=
  let s : State String := ⟨[⟨1, held⟩], []⟩
  "\t".intercalate [held, requested, bit (overlap held requested),
    bit (grants overlap s 2 [requested])]

def fixtureTable : String :=
  "held\trequested\toverlap\tgranted\n" ++
    String.join (patterns.flatMap fun held => patterns.map (fixtureRow held · ++ "\n"))

end Claims.Fixtures

def main : IO Unit := IO.print Claims.Fixtures.fixtureTable
