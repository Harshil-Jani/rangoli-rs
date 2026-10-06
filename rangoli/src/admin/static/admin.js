// Changelist actions: select all, "N of M selected", highlighted rows.
// Runs on first load and after every htmx swap, so live search keeps working.
htmx.onLoad(() => {
  const toggle = document.getElementById("action-toggle");
  const counter = document.querySelector(".action-counter");
  if (!toggle || !counter) return;
  const boxes = () => [...document.querySelectorAll("input.action-select")];
  const update = () => {
    const all = boxes();
    const n = all.filter((b) => b.checked).length;
    counter.textContent = `${n} of ${all.length} selected`;
    all.forEach((b) => b.closest("tr").classList.toggle("selected", b.checked));
    toggle.checked = n > 0 && n === all.length;
  };
  toggle.onchange = () => {
    boxes().forEach((b) => (b.checked = toggle.checked));
    update();
  };
  boxes().forEach((b) => (b.onchange = update));
  update();
});
