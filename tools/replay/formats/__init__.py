# SPDX-License-Identifier: AGPL-3.0-only
"""The formats the replay covers: a module each, registered below.

A format's module has NAME, its repositories' format, and PARTS, what
it brings to the lists that keep an order: each (kind, place, thing),
a list in order of place (a tie in the order of FORMATS, the core's
parts first). Writes change state and the seed's order is the
repositories' ids, so a part keeps its place; today's are tens, in the
order the corpus was first written in. A new part takes a free place,
one past all others' to come last. The kinds:

  seed         f(p, fx): the format's repositories made and filled (p
               the seed's core.Pkgpod, fx the fixtures' directory)
  grants       {user: {repo: role}}: what the seed grants the users
  wtoken       [repo]: of writer's token that writes
  rtoken       [repo]: of writer's token that only reads
  publisher    [repo]: of the trust rule without a user
  stub         f(method, path, auth, bearer): the upstream stub's
               (status, headers, body), or None for a path not its;
               OCI's come first, and on the stub that asks for a token
               they are what /token and, without it, any /v2 path get
  reads        f(c, who): requests that read, as each identity (c the
               core.Corpus)
  reads end    f(c): once, after all identities' reads
  writes       f(c, who): requests that write, as each identity
  writes end   f(c): once, after all identities' writes
  after        f(c, who): reads of what the writes made, as anon and
               as writer
  last         f(c): once, after the fuzz
  rule         (name, what it masks): one of the RULES
  mask         f(masker): for a run, g(path, headers, body, after_writes,
               etag) -> (headers, body), an answer masked, in turn, by
               the rules (counted in masker.counts); path, as served
               (core.served: no runs of slashes, %2f a slash);
               after_writes, whether its request is one of the writes or
               after them
  fuzz sample  bytes: a valid body, for the fuzz to cut and spoil

And, where the format has them: FUZZ, its repositories and the words
their addresses are made of, for the fuzz; FUZZ_WRITABLE, true where
the fuzz may write (an accepted write leaves both runs alike: the run's
times in its answers are masked, or it keeps none); FUZZ_PREFIX, what
its addresses start with before the repository; TIMES, the paths of its
answers that may hold the run's times, matched as served (their parts
are masked as "parts of answers with times", and the roots of its
versions made in the run as "NAME times").

A new format is a module here and a line below. The curated requests
keep their order around its own. The fuzz's are drawn from its FUZZ and
fuzz samples too, and the mutants from its single requests, so most of
them are others; and the tool's hash is another, so the seed is made
again.
"""

from importlib import import_module

FORMATS = [import_module(f"{__name__}.{name}") for name in (
    "files",
    "deb",
    "oci",
    "npm",
    "pypi",
    "cargo",
    "go",
    "gems",
    "composer",
    "pub",
)]
