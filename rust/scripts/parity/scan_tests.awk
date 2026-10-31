# scan_tests.awk — extract test-case keys from a Jest/Vitest-style TS/JS test
# file for the claude-agent-acp parity program (plan-parity-infra item 1.2).
#
# Reads the WHOLE file as ONE awk record (RS="\001"; the codex version's
# `buf = buf $0 "\n"` was quadratic and took 36 s on this export) and walks it
# with a single index, so multi-line it( calls, one-line .each arrays,
# template-literal titles, comments, strings and nested describe() blocks are
# handled across line boundaries.
#
# Emits one line per registered it()/test() case:
#     <file>\t<line>\t<describe-path > title>
#   - <file> from -v file=...
#   - <line> is the 1-based line where the it/test token starts (used by
#     `test-map-check --reconcile` against an independent grep)
#   - <describe-path> is the " > "-joined names of the enclosing describe()
#     blocks; a non-literal describe title becomes "[dynamic-describe#<n>]"
#   - title is the first string-literal argument; a template-literal, an empty
#     literal or any non-string first argument becomes "[dynamic#<n>]"
#   - any `|` in a segment is replaced by `¦` (TEST-MAP uses `|` as separator)
#   - duplicate full keys get "#n" appended (first occurrence bare)
#
# describe() blocks are used only for scoping (their names become part of the
# path through surrounding it()/test() rows); they are not emitted as rows.
#
# .each/.skipIf/.runIf/.for consume a leading argument group that must be
# skipped before the title.  .skip/.only/.todo/.concurrent/.fails take the
# title as the first argument.  A bare `test` preceded by `.` is a regex
# .test( call, not a case, and is ignored.
#
# Formatting is implementation detail; the keys produced must be stable and
# deterministic so the committed snapshot can be regenerated and diffed.

BEGIN {
  RS = "\001"
  file2 = file
  if (file2 ~ /^\.\//) file2 = substr(file2, 3)
}

{ buf = $0 }

END {
  n = length(buf)
  k = 1
  ln = 1
  brace = 0      # brace depth (for describe scoping)
  sp = 0         # describe stack pointer
  dyn = 0        # dynamic-title ordinal (per file)
  dynd = 0       # dynamic-describe ordinal (per file)

  while (k <= n) {
    c = substr(buf, k, 1)
    if (k > 1) prev = substr(buf, k - 1, 1); else prev = ""

    # whitespace
    if (c == " " || c == "\t" || c == "\n" || c == "\r") { advance(1); continue }

    # comments
    if (c == "/" && substr(buf, k + 1, 1) == "/") { skip_line(); continue }
    if (c == "/" && substr(buf, k + 1, 1) == "*") { skip_block(); continue }

    # strings / template literals (non-title context)
    if (c == "\"") { skip_dq(); continue }
    if (c == "'") { skip_sq(); continue }
    if (c == "`") { skip_tmpl(); continue }

    # braces (describe scoping)
    if (c == "{") { brace++; advance(1); continue }
    if (c == "}") {
      brace--
      advance(1)
      while (sp > 0 && brace <= dstack[sp]) sp--
      continue
    }

    # registration opener?  (bounded window so we never copy the whole tail;
    # require a word boundary before the keyword — a preceding `.` means a
    # regex/`x.test(` call, not a registration)
    if ((c == "i" || c == "t" || c == "d") &&
        (k == 1 || prev !~ /[A-Za-z0-9_$.]/)) {
      win = substr(buf, k, 40)
      if (match(win, /^(it|test|describe)(\.[A-Za-z_$][A-Za-z0-9_$]*)?[[:space:]]*\(/)) {
        mlen = RLENGTH
        mtext = substr(win, 1, mlen)
        if (mtext ~ /^describe/) kw = "describe"
        else if (mtext ~ /^test/) kw = "test"
        else kw = "it"
        mod = ""
        if (mtext ~ /^[A-Za-z_$]+[.][A-Za-z_$][A-Za-z0-9_$]*/) {
          dt = index(mtext, ".")
          tmp = substr(mtext, dt + 1)
          if (match(tmp, /^[A-Za-z_$][A-Za-z0-9_$]*/)) mod = substr(tmp, 1, RLENGTH)
        }
        startline = ln
        advance(mlen)   # now just after the "("
        find_title(kw, mod)
        if (got_title) {
          if (kw == "describe") {
            seg = ttitle
            if (seg == "" || tisdyn) {
              dynd++
              seg = "[dynamic-describe#" dynd "]"
            }
            gsub(/\|/, "¦", seg)
            sp++
            dname[sp] = seg
            find_arrow()
            dstack[sp] = dbody
          } else {
            path = ""
            if (sp > 0) {
              for (dd = 1; dd <= sp; dd++) {
                if (dd > 1) path = path " > "
                path = path dname[dd]
              }
            }
            titleout = ttitle
            if (titleout == "" || tisdyn) {
              dyn++
              titleout = "[dynamic#" dyn "]"
            }
            gsub(/\|/, "¦", titleout)
            if (path != "") fulltitle = path " > " titleout
            else fulltitle = titleout
            key = file2 "\t" fulltitle
            cnt = ++seen[key]
            if (cnt > 1) fulltitle = fulltitle "#" cnt
            print file2 "\t" startline "\t" fulltitle
          }
        }
        continue
      }
    }

    # plain identifier / other char
    advance(1)
  }
}

# advance(m): move the cursor m chars forward, keeping the line counter exact.
function advance(m,   i) {
  for (i = 0; i < m; i++) {
    if (substr(buf, k, 1) == "\n") ln++
    k++
  }
}

# ---- low-level helpers (act on global buf, k ; must be followed by `continue`) ----

function skip_line() {
  while (k <= n && substr(buf, k, 1) != "\n") advance(1)
}

function skip_block() {
  advance(2)
  while (k <= n && !(substr(buf, k, 1) == "*" && substr(buf, k + 1, 1) == "/")) advance(1)
  advance(2)
}

function skip_dq() {
  advance(1)
  while (k <= n) {
    cc = substr(buf, k, 1)
    if (cc == "\\") { advance(2); continue }
    if (cc == "\"") { advance(1); break }
    advance(1)
  }
}

function skip_sq() {
  advance(1)
  while (k <= n) {
    cc = substr(buf, k, 1)
    if (cc == "\\") { advance(2); continue }
    if (cc == "'") { advance(1); break }
    advance(1)
  }
}

# skip_tmpl: skip a backtick template literal. A `${...}` interpolation is
# skipped by skip_interp, which in turn skips nested template literals and
# strings, so nested backticks (`outer ${cond ? `inner` : ""}`) are handled.
# Braces inside the interpolation never reach the main walker, so they cannot
# corrupt describe-body brace depth.
function skip_tmpl() {
  advance(1)   # opening backtick
  while (k <= n) {
    cc = substr(buf, k, 1)
    if (cc == "\\") { advance(2); continue }
    if (cc == "`") { advance(1); return }
    if (cc == "$" && substr(buf, k + 1, 1) == "{") { advance(2); skip_interp(); continue }
    advance(1)
  }
}

function skip_interp(   depth, cc) {
  depth = 1
  while (k <= n) {
    cc = substr(buf, k, 1)
    if (cc == "{") { depth++; advance(1); continue }
    if (cc == "}") { depth--; advance(1); if (depth == 0) return; continue }
    if (cc == "\"") { skip_dq(); continue }
    if (cc == "'") { skip_sq(); continue }
    if (cc == "`") { skip_tmpl(); continue }
    advance(1)
  }
}

# read_string: from the current position at an opening double/single quote,
# returns the literal text (without quotes) and leaves k after the closing quote.
function read_string(   qc, out, cc) {
  qc = substr(buf, k, 1)
  advance(1)
  out = ""
  while (k <= n) {
    cc = substr(buf, k, 1)
    if (cc == "\\") { advance(2); continue }
    if (cc == qc) { advance(1); break }
    out = out cc
    advance(1)
  }
  return out
}

# ---- registration title resolution ----
# Called with k positioned just after the "(" that follows the it/test/describe
# keyword+modifier.  Finds the title, sets got_title/ttitle/tisdyn, advances k
# to just after the title token.
function find_title(kw, mod,   skip_lead, rd, islead, cc) {
  got_title = 0
  ttitle = ""
  tisdyn = 0

  skip_lead = (mod == "each" || mod == "skipIf" || mod == "runIf" || mod == "for") ? 1 : 0
  rd = 1            # we are inside the first "(" group already
  islead = skip_lead

  while (1) {
    cc = substr(buf, k, 1)
    if (cc == "") { got_title = 0; return }        # EOF

    if (cc == " " || cc == "\t" || cc == "\n" || cc == "\r") { advance(1); continue }
    if (cc == "/" && substr(buf, k + 1, 1) == "/") { skip_line(); continue }
    if (cc == "/" && substr(buf, k + 1, 1) == "*") { skip_block(); continue }

    if (islead) {
      # inside the leading (each-data / condition) group
      if (cc == "(") { rd++; advance(1); continue }
      if (cc == ")") { rd--; advance(1); if (rd == 0) islead = 0; continue }
      if (cc == "\"") { skip_dq(); continue }
      if (cc == "'") { skip_sq(); continue }
      if (cc == "`") { skip_tmpl(); continue }
      advance(1)
      continue
    }

    # title group
    if (cc == "(") { rd++; advance(1); continue }
    if (cc == ")") { rd--; advance(1); if (rd == 0) { got_title = 0; return } continue }
    if (cc == "\"") { ttitle = read_string(); tisdyn = 0; got_title = 1; return }
    if (cc == "'") { ttitle = read_string(); tisdyn = 0; got_title = 1; return }
    if (cc == "`") { skip_tmpl(); tisdyn = 1; got_title = 1; return }
    # non-literal first argument (identifier / expression) -> dynamic
    tisdyn = 1
    got_title = 1
    return
  }
}

# ---- describe body detection ----
# Called with k positioned just after the describe title token (and inside the
# describe call). Scans the remaining scalar/object arguments and the callback
# opening until it reaches the callback "=>" (arrow). Sets dbody = the brace
# depth at the arrow (the depth its body block returns to on close) and leaves k
# just after "=>". Any {timeout} object-literal args before the arrow are
# consumed and balanced here, so the main loop's pop test never mistakes their
# closing brace for the describe's body close. If no arrow is found (a `() {`
# style callback absent in this codebase) dbody is set so the describe never
# pops, keeping paths stable rather than corrupting the stack.
function find_arrow(   cc) {
  dbody = -999999
  while (1) {
    cc = substr(buf, k, 1)
    if (cc == "") return
    if (cc == " " || cc == "\t" || cc == "\n" || cc == "\r") { advance(1); continue }
    if (cc == "/" && substr(buf, k + 1, 1) == "/") { skip_line(); continue }
    if (cc == "/" && substr(buf, k + 1, 1) == "*") { skip_block(); continue }
    if (cc == "\"") { skip_dq(); continue }
    if (cc == "'") { skip_sq(); continue }
    if (cc == "`") { skip_tmpl(); continue }
    if (cc == "=" && substr(buf, k + 1, 1) == ">") { dbody = brace; advance(2); return }
    if (cc == "{") { brace++; advance(1); continue }
    if (cc == "}") { brace--; advance(1); continue }
    advance(1)
  }
}
