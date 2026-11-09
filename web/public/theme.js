(function () {
  try {
    var stored = localStorage.getItem('phoebis.theme');
    var mode = stored === 'light' || stored === 'dark' ? stored : null;
    if (!mode) {
      mode = window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    }
    var palette = localStorage.getItem('phoebis.palette') === 'github' ? 'github' : 'default';
    document.documentElement.setAttribute('data-theme', mode);
    document.documentElement.setAttribute('data-palette', palette);
  } catch {
    document.documentElement.setAttribute('data-theme', 'light');
    document.documentElement.setAttribute('data-palette', 'default');
  }
})();
