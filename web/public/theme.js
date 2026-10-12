(function () {
  try {
    var stored = localStorage.getItem('phoebis.theme');
    var mode = stored === 'light' || stored === 'dark' ? stored : null;
    if (!mode) {
      mode = window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    }
    document.documentElement.setAttribute('data-theme', mode);
  } catch {
    document.documentElement.setAttribute('data-theme', 'light');
  }
})();
