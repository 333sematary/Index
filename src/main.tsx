import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { Braces, Check, ChevronDown, Code2, File, Folder, FolderPlus, Minus, Search, Settings2, X } from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./style.css";

type Project = { id: number; name: string; path: string; file_count: number; indexed_at: number | null };
type Hit = { path: string; relative_path: string; name: string; extension: string; line: number; snippet: string; project_id: number; project_name: string; result_type: "file" | "symbol" | "text"; symbol_kind?: string | null; symbol_name?: string | null };
type Preferences = { editor: string; executable: string; shortcut: string; shortcut_error?: string | null; watcher_error?: string | null };
type SearchMode = "files" | "symbols" | "content";
type SearchResponse = { results: Hit[]; total: number };
const extensions = ["rs","ts","tsx","js","jsx","py","go","java","kt","c","cpp","cs","php","rb","swift","sql","html","css","json","toml","yaml","md","vue","svelte","lua","dart"];

export default function App() {
  const [projects, setProjects] = useState<Project[]>([]);
  const [activeProject, setActiveProject] = useState<number | null>(null);
  const [query, setQuery] = useState("");
  const [extension, setExtension] = useState("");
  const [mode, setMode] = useState<SearchMode>("content");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [hits, setHits] = useState<Hit[]>([]);
  const [total, setTotal] = useState(0);
  const [busy, setBusy] = useState(false);
  const [indexing, setIndexing] = useState(false);
  const [selected, setSelected] = useState(0);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [projectMenu, setProjectMenu] = useState(false);
  const [projectBusy, setProjectBusy] = useState<number | null>(null);
  const [settings, setSettings] = useState(false);
  const [preferences, setPreferences] = useState<Preferences>({ editor: "auto", executable: "", shortcut: "CTRL+SHIFT+SPACE" });
  const inputRef = useRef<HTMLInputElement>(null);
  const resultsRef = useRef<HTMLDivElement>(null);
  const runRef = useRef(0);

  const loadProjects = useCallback(async () => {
    try {
      const list = await invoke<Project[]>("list_projects");
      setProjects(list);
      setActiveProject(current => current && list.some(project => project.id === current) ? current : null);
    } catch (reason) { setError(String(reason)); }
  }, []);
  useEffect(() => { void loadProjects(); }, [loadProjects]);
  useEffect(() => { void invoke<Preferences>("get_preferences").then(value => { setPreferences(value); if (value.shortcut_error || value.watcher_error) setError(value.shortcut_error || value.watcher_error || ""); }).catch(reason => setError(String(reason))); }, []);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen("global-focus-search", () => { inputRef.current?.focus(); inputRef.current?.select(); }).then(fn => { unlisten = fn; });
    return () => unlisten?.();
  }, []);
  useEffect(() => {
    const stops: Array<() => void> = [];
    void listen<number>("index-started", () => setIndexing(true)).then(fn => stops.push(fn));
    void listen<Project>("index-updated", ({ payload }) => { setProjects(current => current.map(project => project.id === payload.id ? payload : project)); setIndexing(false); setNotice(`${payload.name} index updated`); }).then(fn => stops.push(fn));
    void listen<string>("index-error", ({ payload }) => { setError(`Index update failed: ${payload}`); setIndexing(false); }).then(fn => stops.push(fn));
    return () => stops.forEach(stop => stop());
  }, []);

  const runSearch = useCallback(async () => {
    const run = ++runRef.current;
    if (!query.trim()) { setHits([]); setTotal(0); setBusy(false); return; }
    setBusy(true);
    try {
      const data = await invoke<SearchResponse>("search", { options: { query, projectId: activeProject, extension: extension || null, caseSensitive, mode } });
      if (run === runRef.current) { setHits(data.results); setTotal(data.total); setSelected(0); setError(""); }
    } catch (reason) { if (run === runRef.current) { setError(String(reason)); setHits([]); setTotal(0); } }
    finally { if (run === runRef.current) setBusy(false); }
  }, [query, activeProject, extension, caseSensitive, mode]);
  useEffect(() => { const timer = window.setTimeout(() => void runSearch(), 130); return () => window.clearTimeout(timer); }, [runSearch]);
  useEffect(() => { resultsRef.current?.querySelector(`[data-index="${selected}"]`)?.scrollIntoView({ block: "nearest" }); }, [selected]);
  useEffect(() => { window.setTimeout(() => inputRef.current?.focus(), 80); }, []);

  const addFolder = async () => {
    const picked = await open({ directory: true, multiple: false, title: "Choose a project folder" });
    if (!picked || Array.isArray(picked)) return;
    setProjectMenu(false); setError(""); setNotice(""); setIndexing(true);
    try {
      const project = await invoke<Project>("add_project", { path: picked });
      await loadProjects(); setActiveProject(project.id); setNotice(`${project.file_count.toLocaleString()} files indexed`);
    } catch (reason) { setError(String(reason)); }
    finally { setIndexing(false); }
  };
  const refreshProject = async (project: Project) => {
    setProjectBusy(project.id); setError("");
    try { const refreshed = await invoke<Project>("refresh_project", { id: project.id }); await loadProjects(); setNotice(`${refreshed.file_count.toLocaleString()} files indexed`); }
    catch (reason) { setError(String(reason)); }
    finally { setProjectBusy(null); }
  };
  const removeProject = async (project: Project) => {
    try { await invoke("remove_project", { id: project.id }); await loadProjects(); setNotice(`${project.name} removed`); }
    catch (reason) { setError(String(reason)); }
  };
  const openHit = (hit: Hit) => { void invoke<string | null>("open_file", { path: hit.path, line: hit.line }).then(message => { if (message) setNotice(message); }).catch(reason => setError(String(reason))); };
  const savePrefs = async () => {
    try { await invoke("save_preferences", { preferences }); setNotice("Settings saved"); setError(""); }
    catch (reason) { setError(String(reason)); }
  };
  const onKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown" && hits.length) { event.preventDefault(); setSelected(index => Math.min(index + 1, hits.length - 1)); }
    if (event.key === "ArrowUp" && hits.length) { event.preventDefault(); setSelected(index => Math.max(index - 1, 0)); }
    if (event.key === "Enter" && hits[selected]) { event.preventDefault(); openHit(hits[selected]); }
    if (event.key === "Escape") { if (projectMenu || settings) { setProjectMenu(false); setSettings(false); } else { setQuery(""); setHits([]); setTotal(0); } }
  };
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") { event.preventDefault(); inputRef.current?.focus(); }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "p") { event.preventDefault(); setProjectMenu(value => !value); }
    };
    window.addEventListener("keydown", onKey); return () => window.removeEventListener("keydown", onKey);
  }, []);

  const selectedProject = projects.find(project => project.id === activeProject);
  return <div className="palette">
    <header className="palette-header" onMouseDown={event => { if (event.button !== 0 || (event.target as HTMLElement).closest("button,input,select")) return; void getCurrentWindow().startDragging().catch(reason => setError(String(reason))); }}>
      <div className="identity"><span className="index-mark" aria-hidden="true"><Search size={17} strokeWidth={1.8}/></span><span className="wordmark">index</span></div>
      <div className="header-actions">
        <div className="project-control">
          <button className={`project-trigger ${projectMenu ? "open" : ""}`} onClick={() => { setProjectMenu(value => !value); setSettings(false); }} title="Choose project (Ctrl P)"><Folder size={13}/><span>{selectedProject?.name ?? "All projects"}</span><ChevronDown size={12}/></button>
          {projectMenu && <div className="project-popover">
            <button className={!activeProject ? "project-option active" : "project-option"} onClick={() => { setActiveProject(null); setProjectMenu(false); }}><span className="project-radio">{!activeProject && <Check size={10}/>}</span><span>All projects</span><span className="option-count">{projects.length}</span></button>
            {projects.map(project => <div className="project-option-wrap" key={project.id}>
              <button className={`project-option ${activeProject === project.id ? "active" : ""}`} onClick={() => { setActiveProject(project.id); setProjectMenu(false); }} title={project.path}><span className="project-radio">{activeProject === project.id && <Check size={10}/>}</span><span className="option-name">{project.name}</span><span className="option-count">{project.file_count}</span></button>
              <div className="project-manage"><button title="Refresh project index" onClick={() => void refreshProject(project)}>{projectBusy === project.id ? "…" : "↻"}</button><button title="Remove project" onClick={() => void removeProject(project)}>×</button></div>
            </div>)}
            <div className="popover-separator"/><button className="add-project" onClick={() => void addFolder()}><FolderPlus size={13}/> Add project folder</button>
          </div>}
        </div>
        <button className={`header-icon ${settings ? "on" : ""}`} title="Settings" aria-label="Settings" onClick={() => { setSettings(value => !value); setProjectMenu(false); }}><Settings2 size={14}/></button>
        <button className="header-icon window-action" title="Minimize" aria-label="Minimize" onClick={() => void getCurrentWindow().minimize().catch(reason => setError(String(reason)))}><Minus size={14}/></button>
        <button className="header-icon window-action close-action" title="Close" aria-label="Close" onClick={() => void getCurrentWindow().close().catch(reason => setError(String(reason)))}><X size={14}/></button>
      </div>
    </header>

    <main className="palette-main">
      <div className="search-box"><Search className="search-icon" size={17}/><input ref={inputRef} value={query} onChange={event => setQuery(event.target.value)} onKeyDown={onKeyDown} placeholder="Search files, symbols, and code..." autoComplete="off" spellCheck={false}/>{query && <button className="clear-query" aria-label="Clear search" onClick={() => { setQuery(""); inputRef.current?.focus(); }}><X size={13}/></button>}</div>
      <div className="control-row">
        <div className="mode-tabs" role="tablist" aria-label="Search mode">{(["files", "symbols", "content"] as const).map(item => <button key={item} role="tab" aria-selected={mode === item} className={mode === item ? "mode-tab active" : "mode-tab"} onClick={() => setMode(item)}>{item === "files" ? <File size={12}/> : item === "symbols" ? <Braces size={12}/> : <Code2 size={12}/>}<span>{item[0].toUpperCase() + item.slice(1)}</span></button>)}</div>
        <div className="search-options"><select aria-label="Filter by file extension" value={extension} onChange={event => setExtension(event.target.value)}><option value="">Any type</option>{extensions.map(ext => <option key={ext} value={ext}>.{ext}</option>)}</select><button className={`case-toggle ${caseSensitive ? "active" : ""}`} title="Toggle case sensitivity" aria-label="Match case" onClick={() => setCaseSensitive(value => !value)}>Aa</button></div>
      </div>
      <section className="result-area" ref={resultsRef} aria-live="polite">
        {(error || notice) && <div className={error ? "inline-message error" : "inline-message"}><span>{error || notice}</span><button onClick={() => { setError(""); setNotice(""); }} aria-label="Dismiss message"><X size={12}/></button></div>}
        {settings && <div className="settings-popover"><div className="settings-heading">Settings</div><label className="setting-form">Editor<select value={preferences.editor} onChange={event => setPreferences({...preferences,editor:event.target.value})}><option value="auto">Automatic (VS Code / Visual Studio)</option><option value="vscode">VS Code</option><option value="visualstudio">Visual Studio</option><option value="nvim">Neovim</option></select></label><label className="setting-form">Executable path<input value={preferences.executable} placeholder={preferences.editor === "nvim" ? "nvim" : preferences.editor === "visualstudio" ? "devenv.exe" : preferences.editor === "auto" ? "Auto detect installed editor" : "code"} onChange={event => setPreferences({...preferences,executable:event.target.value})}/></label><label className="setting-form">Global shortcut<input value={preferences.shortcut} onChange={event => setPreferences({...preferences,shortcut:event.target.value})}/></label><div className="setting-note">Automatic mode checks VS Code, then Visual Studio. Blank executable uses automatic detection.</div><button className="settings-save" onClick={() => void savePrefs()}>Save settings</button><div className="setting-line"><span>Storage</span><span>Local SQLite</span></div></div>}
        {!query.trim() && !busy && !error && indexing && <div className="empty-hint">Indexing project…</div>}
        {!query.trim() && !busy && !error && !indexing && <div className="empty-hint"><span>{projects.length ? "Search your indexed code" : "Choose a folder to start"}</span><span className="hint-secondary">{mode === "symbols" ? "Symbols: Rust · TypeScript · JavaScript · Lua" : projects.length ? "Results will appear here" : "Local index · no uploads"}</span>{!projects.length && <button onClick={() => void addFolder()}><FolderPlus size={12}/> Add project</button>}</div>}
        {query.trim() && busy && !hits.length && <div className="empty-hint"><span className="working-mark"/>Searching locally…</div>}
        {query.trim() && !busy && !hits.length && !error && <div className="empty-hint"><span>No {mode} found for “{query}”</span><span className="hint-secondary">{mode === "symbols" ? "Tree-sitter symbols are available for Rust, TypeScript, JavaScript, and Lua" : "Try a shorter query or change the project filter"}</span></div>}
        {hits.map((hit, index) => <button key={`${hit.path}:${hit.line}`} data-index={index} className={`result-row ${selected === index ? "selected" : ""}`} onMouseEnter={() => setSelected(index)} onClick={() => openHit(hit)}>
          <span className={`file-glyph ext-${hit.extension}`}>{hit.extension === "rs" ? "R" : hit.extension === "py" ? "Py" : hit.extension === "ts" || hit.extension === "tsx" ? "TS" : hit.extension.slice(0, 2).toUpperCase() || "·"}</span>
          <span className="result-copy"><span className="result-heading"><span className="result-name">{mode === "symbols" && hit.symbol_name ? hit.symbol_name : hit.name}</span><span className="result-path">{mode === "symbols" ? `${hit.name} · ${hit.relative_path}` : hit.relative_path}</span></span>{mode !== "files" && <span className="result-snippet">{hit.snippet}</span>}</span>
          <span className="result-meta"><span>{hit.symbol_kind || hit.result_type}</span><span>.{hit.extension || "file"}</span><span>:{hit.line}</span></span>
        </button>)}
        {total > hits.length && <div className="more-results">Showing {hits.length} of {total} matches</div>}
      </section>
    </main>

  </div>;
}
