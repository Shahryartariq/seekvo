import { DiskMap } from "@/components/DiskMap";
import { SearchDemo } from "@/components/SearchDemo";
import { DUPLICATE_GROUPS, RENAME_SUGGESTIONS } from "@/lib/sample-drive";

const REPO_URL = "https://github.com/Shahryartariq/seekvo";

const STAGES = [
  {
    title: "Search",
    body: "Read the drive's own index and list any file by name as you type. Goal for the first release: a 1 TB drive indexed in under 10 seconds.",
  },
  {
    title: "Disk map and copies",
    body: "Draw every folder to scale, find exact copies, and move them to a holding folder you can restore from.",
  },
  {
    title: "Tidy up",
    body: "Suggest names and folders using an AI model that runs on your PC. Every change is shown first and can be undone.",
  },
  {
    title: "Search by meaning",
    body: "Find a file by describing it, and let AI assistants such as Claude search your files through Seekvo, with your permission.",
  },
];

/** A Windows path that may wrap after any backslash. */
function WinPath({ value }: { value: string }) {
  const parts = value.split("\\");
  return (
    <>
      {parts.map((part, index) => (
        <span key={index}>
          {part}
          {index < parts.length - 1 ? (
            <>
              {"\\"}
              <wbr />
            </>
          ) : null}
        </span>
      ))}
    </>
  );
}

export default function Home() {
  return (
    <>
      <header className="top wrap">
        <p className="wordmark">Seekvo</p>
        <a href={REPO_URL}>GitHub</a>
      </header>

      <main id="main">
        <section className="hero wrap">
          <h1>Find any file on your PC before you finish typing.</h1>
          <p className="lede">
            Seekvo is a search and cleanup app for Windows. It reads the index your drive already
            keeps, so results show up as you type. It is in development, and there is nothing to
            download yet.
          </p>
          <SearchDemo />
        </section>

        <section className="band" aria-labelledby="map-title">
          <div className="wrap split">
            <div className="split-text">
              <h2 id="map-title">See where the space went.</h2>
              <p>
                Seekvo will draw every folder to scale. The largest ones are the first thing you
                see, so a full drive stops being a mystery.
              </p>
            </div>
            <DiskMap />
          </div>
        </section>

        <section className="wrap split" aria-labelledby="copies-title">
          <div className="split-text">
            <h2 id="copies-title">Clear out the copies.</h2>
            <p>
              Files are compared by what is inside them, not by name. Extra copies go to a holding
              folder first, so you can put any of them back.
            </p>
          </div>
          <div className="copies">
            <ul className="copies-list">
              {DUPLICATE_GROUPS.map((group) => (
                <li key={group.name}>
                  <p className="copies-head">
                    <span className="path">{group.name}</span>
                    <span>
                      {group.locations.length} copies, {group.each} each
                    </span>
                  </p>
                  <ul className="copies-places">
                    {group.locations.map((location, index) => (
                      <li key={location}>
                        <span className={index === 0 ? "tag tag-keep" : "tag"}>
                          {index === 0 ? "Keep" : "Copy"}
                        </span>
                        <span className="path">
                          <WinPath value={location} />
                        </span>
                      </li>
                    ))}
                  </ul>
                </li>
              ))}
            </ul>
            <p className="sample-note">Sample results from a made-up drive.</p>
          </div>
        </section>

        <section className="wrap split" aria-labelledby="tidy-title">
          <div className="split-text">
            <h2 id="tidy-title">Tidy up, with your <span className="nowrap">say-so.</span></h2>
            <p>
              An AI model on your own PC reads what a file is and suggests a name and a folder. You
              see every change before it happens, and you can undo it afterwards.
            </p>
          </div>
          <div className="tidy">
            <table>
              <thead>
                <tr>
                  <th scope="col">Now</th>
                  <th scope="col">Suggested</th>
                </tr>
              </thead>
              <tbody>
                {RENAME_SUGGESTIONS.map((row) => (
                  <tr key={row.before}>
                    <td className="path">{row.before}</td>
                    <td>
                      <span className="path tidy-after">{row.after}</span>
                      <span className="path tidy-folder">
                        <WinPath value={row.folder} />
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="sample-note">Sample suggestions. Nothing moves until you approve it.</p>
          </div>
        </section>

        <section className="wrap private" aria-labelledby="private-title">
          <h2 id="private-title">Nothing leaves your PC.</h2>
          <ul className="private-list">
            <li>
              <h3>Works offline</h3>
              <p>The index and the AI models stay on your machine. No file is uploaded.</p>
            </li>
            <li>
              <h3>No account</h3>
              <p>You install it and use it. There is no sign-up and no usage tracking.</p>
            </li>
            <li>
              <h3>You approve changes</h3>
              <p>Seekvo suggests. Renames, moves and cleanups happen only when you say so.</p>
            </li>
          </ul>
        </section>

        <section className="wrap stages" aria-labelledby="stages-title">
          <h2 id="stages-title">Where the build stands.</h2>
          <p className="stages-intro">
            Seekvo is at the design stage. It will be built in this order, and each stage ships
            when it works.
          </p>
          <ol className="stages-list">
            {STAGES.map((stage) => (
              <li key={stage.title}>
                <h3>{stage.title}</h3>
                <p>{stage.body}</p>
              </li>
            ))}
          </ol>
        </section>

        <section className="wrap follow" aria-labelledby="follow-title">
          <h2 id="follow-title">Follow the build.</h2>
          <p>Releases will be announced on GitHub. Watch the repository to hear about the first one.</p>
          <a className="button" href={REPO_URL}>
            Watch Seekvo on GitHub
          </a>
        </section>
      </main>

      <footer className="wrap foot">
        <p className="foot-line">
          Seekvo is being built by{" "}
          <a href="https://github.com/Shahryartariq">Muhammad Shehryar</a>.
        </p>
      </footer>
    </>
  );
}
