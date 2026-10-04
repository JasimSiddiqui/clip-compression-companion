// Runs before the page paints, so it never flashes the wrong theme: the
// remembered choice if there is one, otherwise whatever the system uses.
(function () {
  var saved = null;
  try {
    saved = localStorage.getItem("ccc-theme");
  } catch (e) {}
  var theme = saved || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  document.documentElement.setAttribute("data-theme", theme);
})();
