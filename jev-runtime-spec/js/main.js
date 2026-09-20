(function () {
  const navLinks = Array.from(document.querySelectorAll(".nav-list a[href^='#']"));
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
        const visible = entries
          .filter((e) => e.isIntersecting)
          .sort((a, b) => b.intersectionRatio - a.intersectionRatio);
        if (visible[0]) setCurrent(visible[0].target.id);
      },
      { rootMargin: "-20% 0px -55% 0px", threshold: [0.1, 0.25, 0.5] }
    );
    sections.forEach((s) => io.observe(s));
  }

  // EPIC filter
  const filterBar = document.getElementById("epic-filter");
  const cards = Array.from(document.querySelectorAll("[data-epic-card]"));
  const empty = document.getElementById("epic-empty");

  if (filterBar && cards.length) {
    filterBar.addEventListener("click", (e) => {
      const btn = e.target.closest("button[data-filter]");
      if (!btn) return;
      const key = btn.getAttribute("data-filter");
      filterBar.querySelectorAll("button[data-filter]").forEach((b) => {
        b.setAttribute("aria-pressed", b === btn ? "true" : "false");
      });
      let shown = 0;
      cards.forEach((card) => {
        const match = key === "all" || card.getAttribute("data-lane") === key;
        card.hidden = !match;
        if (match) shown += 1;
      });
      if (empty) empty.classList.toggle("show", shown === 0);
    });
  }
})();
