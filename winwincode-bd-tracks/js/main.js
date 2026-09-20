(function () {
  const navLinks = Array.from(document.querySelectorAll('.nav-list a[href^="#"]'));
  const sections = navLinks
    .map((a) => document.querySelector(a.getAttribute("href")))
    .filter(Boolean);

  function setCurrent(id) {
    navLinks.forEach((a) => {
      const on = a.getAttribute("href") === "#" + id;
      if (on) a.setAttribute("aria-current", "true");
      else a.removeAttribute("aria-current");
    });
  }

  if ("IntersectionObserver" in window && sections.length) {
    const io = new IntersectionObserver(
      (entries) => {
        const vis = entries
          .filter((e) => e.isIntersecting)
          .sort((a, b) => b.intersectionRatio - a.intersectionRatio);
        if (vis[0]) setCurrent(vis[0].target.id);
      },
      { rootMargin: "-20% 0px -55% 0px", threshold: [0.1, 0.25, 0.5] }
    );
    sections.forEach((s) => io.observe(s));
  }

  const bar = document.getElementById("bd-filter");
  const cards = Array.from(document.querySelectorAll("[data-bd]"));
  const empty = document.getElementById("bd-empty");
  if (!bar || !cards.length) return;

  bar.addEventListener("click", (e) => {
    const btn = e.target.closest("button[data-filter]");
    if (!btn) return;
    const key = btn.getAttribute("data-filter");
    bar.querySelectorAll("button[data-filter]").forEach((b) => {
      b.setAttribute("aria-pressed", b === btn ? "true" : "false");
    });
    let shown = 0;
    cards.forEach((card) => {
      const match = key === "all" || card.getAttribute("data-track") === key;
      card.hidden = !match;
      if (match) shown += 1;
    });
    if (empty) empty.classList.toggle("show", shown === 0);
  });
})();
