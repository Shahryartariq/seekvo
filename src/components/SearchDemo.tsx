"use client";

import { useState } from "react";
import { SAMPLE_FILES, type SampleFile } from "@/lib/sample-files";
import { formatSize, highlight, parseQuery, search, type Query } from "@/lib/search";

const LIMIT = 8;
const START_QUERY = "invoice";
const EXAMPLES = ["invoice march", "ext:mp4", "report 2026", "node_modules react", "hunza"];

type State = {
  text: string;
  query: Query;
  total: number;
  hits: SampleFile[];
  /** How long the search took in milliseconds; null before the first keystroke. */
  took: number | null;
};

function run(text: string, timed: boolean): State {
  const query = parseQuery(text);
  const started = timed ? performance.now() : 0;
  const { total, hits } = search(SAMPLE_FILES, query, LIMIT);
  const took = timed ? performance.now() - started : null;
  return { text, query, total, hits, took };
}

function describeTime(took: number): string {
  if (took < 1) return "in under a millisecond";
  return `in ${took.toFixed(1)} ms`;
}

function Marked({ text, terms }: { text: string; terms: readonly string[] }) {
  return (
    <>
      {highlight(text, terms).map((segment, index) =>
        segment.hit ? <mark key={index}>{segment.text}</mark> : <span key={index}>{segment.text}</span>,
      )}
    </>
  );
}

export function SearchDemo() {
  const [state, setState] = useState<State>(() => run(START_QUERY, false));
  const sampleCount = SAMPLE_FILES.length.toLocaleString("en-US");
  const hasQuery = state.query.terms.length > 0 || state.query.ext !== null;

  let status: string;
  if (!hasQuery) {
    status = `${sampleCount} sample files ready. Start typing.`;
  } else if (state.total === 0) {
    status = "No sample file matches that. Try invoice, ext:mp4 or report 2026.";
  } else {
    const count = state.total.toLocaleString("en-US");
    const noun = state.total === 1 ? "match" : "matches";
    const timing = state.took === null ? "" : ` ${describeTime(state.took)}`;
    status = `${count} ${noun} in ${sampleCount} sample files${timing}`;
  }

  return (
    <div className="demo">
      <label className="demo-label" htmlFor="demo-query">
        Try it on a sample drive
      </label>
      <input
        id="demo-query"
        className="demo-field"
        type="search"
        value={state.text}
        onChange={(event) => setState(run(event.target.value, true))}
        placeholder="Type part of a file name"
        autoComplete="off"
        autoCapitalize="off"
        autoCorrect="off"
        spellCheck={false}
        enterKeyHint="search"
      />

      <ul className="demo-examples" aria-label="Example searches">
        {EXAMPLES.map((example) => (
          <li key={example}>
            <button type="button" onClick={() => setState(run(example, true))}>
              {example}
            </button>
          </li>
        ))}
      </ul>

      <p className="demo-status" role="status">
        {status}
      </p>

      <ol className="demo-results">
        {state.hits.map((file) => (
          <li key={file.dir + file.name}>
            <span className="demo-path">
              <span className="demo-dir">
                <Marked text={file.dir} terms={state.query.terms} />
              </span>
              <span className="demo-name">
                <Marked text={file.name} terms={state.query.terms} />
              </span>
            </span>
            <span className="demo-size">{formatSize(file.size)}</span>
          </li>
        ))}
      </ol>

      {state.total > state.hits.length ? (
        <p className="demo-more">Showing the first {state.hits.length}.</p>
      ) : null}

      <p className="demo-note">
        This demo searches made-up file names inside your browser. The app will read the index of
        your own NTFS drives instead.
      </p>
    </div>
  );
}
