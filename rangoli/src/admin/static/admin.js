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

// Conditional fields (ModelAdmin::show_when): show a field only while its controller holds
// one of the listed values. The server ignores hidden fields on save, so this is display only.
htmx.onLoad(() => {
  const form = document.querySelector("form[id$=_form]");
  const deps = [...document.querySelectorAll("[data-show-when]")];
  if (!form || !deps.length) return;
  const value = (name) => {
    const el = form.elements[name];
    if (!el) return "";
    return el.type === "checkbox" ? String(el.checked) : el.value;
  };
  const update = () =>
    deps.forEach((d) => (d.hidden = !d.dataset.showValues.split("|").includes(value(d.dataset.showWhen))));
  form.addEventListener("input", update);
  form.addEventListener("change", update);
});
