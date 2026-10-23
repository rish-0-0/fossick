import SearchPage from './components/SearchPage'

// One tab today; the sidebar is the navigation surface as more arrive.
const TABS = [{ id: 'search', label: 'Search' }]

export default function App() {
  const active = 'search'
  return (
    <div className="app">
      <header className="navbar">
        <span className="brand">⛏ fossick</span>
        <span className="navbar-sub">log search</span>
      </header>
      <div className="body">
        <nav className="sidebar">
          {TABS.map((t) => (
            <a key={t.id} className={t.id === active ? 'nav-item active' : 'nav-item'} href="/">
              {t.label}
            </a>
          ))}
        </nav>
        <main className="content">
          <SearchPage />
        </main>
      </div>
    </div>
  )
}
