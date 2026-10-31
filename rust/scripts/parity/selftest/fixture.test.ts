// Selftest fixture for rust/scripts/parity/test-map-check --selftest and
// rust/claude-agent-acp-rs/tests/parity_maps.rs. Every construct the extractor
// must handle is present; the expected rows are embedded in test-map-check.
const someClient = "zed";
declare const x: string;

// template-literal title -> [dynamic#1]
it(`template literal title with interpolation ${someClient}`, () => {});

// multi-line call and multi-line title
it(
  "multi.line call spanning several lines",
  () => {}
);

// .each takes a leading data group before the title (one row per site)
it.each([["a"], ["b"]] as const)("parametrized case for %s", (v) => {});

// .skipIf takes a leading condition group
it.skipIf(process.env.CI === "1")("conditional case", () => {});

// plain test()
test("plain test call", () => {});

// nested describes
describe("outer", () => {
  describe("inner", () => {
    it("nested case", () => {});
  });
});

// describe.each: the title is a literal (the %s is substituted per row)
describe.each([["x"], ["y"]] as const)("%s group", () => {
  it("each-describe case", () => {});
});

describe.skipIf(process.env.CI === "1")("skipped describe", () => {
  it("skipif case", () => {});
});

describe.runIf(process.env.CI !== "1")("runif describe", () => {
  it("runif case", () => {});
});

// a describe whose title is chosen at runtime -> [dynamic-describe#1]
const chosenName = "chosen";
describe(chosenName, () => {
  it("inside dynamic describe", () => {});
});

// a case registered inside a helper function
function registerCases() {
  it("registered in helper", () => {});
}

// cases registered inside a loop (template titles -> [dynamic#2], [dynamic#3])
for (const v of ["a", "b"]) {
  it(`loop ${v}`, () => {});
}

// duplicate titles -> the second gets #2
it("duplicate title", () => {});
it("duplicate title", () => {});

// a pipe in a title is escaped to ¦
it("pipes | in title", () => {});

// a regex .test( call is NOT a case (the keyword is preceded by `.`)
expect("x").test(/y/);

// a commented-out registration is NOT a case
// it("commented out", () => {});

// a string containing a registration is NOT a case
const notACase = "it(\"string case\")";

// an empty literal title -> [dynamic#4] (never an empty path segment)
it("", () => {});
