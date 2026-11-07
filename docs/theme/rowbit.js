// The docs' title links back to the home page, and the bar gets the same
// links as the home page's header, so the two read as one site.
(function () {
  var home = "/rowbit/";
  var title = document.querySelector(".menu-title");
  if (title) {
    title.textContent = "";
    var a = document.createElement("a");
    a.className = "rowbit-home";
    a.href = home;
    a.title = "Rowbit home";
    a.append("ROWBIT");
    var docs = document.createElement("span");
    docs.className = "rowbit-docs";
    docs.textContent = "DOCS";
    a.append(docs);
    title.append(a);
  }
  var right = document.querySelector(".right-buttons");
  if (right) {
    var nav = document.createElement("nav");
    nav.className = "rowbit-nav";
    [
      ["Home", home],
      ["Get started", home + "#start-h"],
      ["Changelog", "https://github.com/Ermag/rowbit/blob/main/CHANGELOG.md"],
    ].forEach(function (l) {
      var a = document.createElement("a");
      a.href = l[1];
      a.textContent = l[0];
      nav.append(a);
    });
    right.prepend(nav);
  }
})();
