// Schedule microtask (wait until all synchrounous work is done)
const deferOnce = (obj, fn) => {
  obj.$$defer =
    obj.$$defer ||
    Promise.resolve()
      .then(() => fn())
      .finally(() => delete obj.$$defer);
};

const debounce = (fn, timeoutMs = 500) => {
  let id;
  return () => {
    if (id) window.clearTimeout(id);
    id = window.setTimeout(() => fn(), timeoutMs);
  };
};

const livewire = (props) => {
  const live = Object.entries(props).filter(([k, _fn]) => k.startsWith("$"));
  const state = { ...props };
  const watchers = [];

  const derive = () => live.forEach(([key, fn]) => (state[key] = fn(state)));

  // Populate initial live values
  derive();

  return new Proxy(state, {
    set(target, _key, _value) {
      Reflect.set(...arguments);
      derive();

      deferOnce(this, () => watchers.forEach((fn) => fn(target)));
      return true;
    },

    get(target, key, proxy) {
      if (key === "watch") {
        return (...args) => {
          let fn = args[0];

          // Only watch for changes in specific properties
          if (args.length === 2) {
            const [watchedKeys, wrappedFn] = args;

            fn = (obj) => {
              const cur = JSON.stringify(
                Object.entries(obj).filter(([k, _]) => watchedKeys.includes(k)),
              );

              if (cur === wrappedFn.$$cache) return;
              wrappedFn.$$cache = cur;
              wrappedFn(obj);
            };
          }

          // invoke the function once with current state
          deferOnce(fn, () => fn(target));
          watchers.push(fn);

          return proxy;
        };
      }
      return Reflect.get(...arguments);
    },
  });
};

function _createElement(tag, attrs, children) {
  attrs = attrs || {};
  let el = document.createElement(tag);

  for (const [k, v] of Object.entries(attrs)) {
    if (typeof v === "function") {
      el.addEventListener(k, v.bind(this));
    } else if (k === "style" && typeof v === "object") {
      Object.entries(v).forEach(([k, v]) => el.style.setProperty(k, v));
    } else {
      el.setAttribute(k, v);
    }
  }

  const nodes = [children]
    .flat(Infinity)
    .filter((it) => it != null)
    .map((it) =>
      it instanceof Node ? it : document.createTextNode(it.toString()),
    );

  el.append(...nodes);
  return el;
}

// magic curry sauce
const createElement = new Proxy(_createElement, {
  get: (_target, prop, receiver) => (attrs, children) =>
    receiver(prop, attrs, children),
});

// {a: "foo", b: null, c: " "} => "a=foo&c=%20"
function encodeQueryString(obj) {
  return Object.entries(obj)
    .filter(([_k, v]) => v != null && v !== "")
    .map((kv) => kv.map(encodeURIComponent))
    .map(([k, v]) => `${k}=${v}`)
    .join("&");
}

function createUploadModal() {
  const { div, form, input, label, p, code } = createElement;

  const resultContainer = div({ class: "__results" });
  const progressBar = div({ class: "__progress" });

  const uploader = new FileUploader({
    onProgress({ fileName, success, message, progress }) {
      resultContainer.style.display = "block";
      progressBar.style.opacity = 1;
      progressBar.style.width = `${progress}%`;
      resultContainer.prepend(
        div({ class: `__row ${success ? "--success" : "--error"}` }, [
          div({ class: "__file", title: fileName }, fileName),
          message,
        ]),
      );
    },

    onComplete() {
      progressBar.style.opacity = 0;
    },
  });

  const dragHandler = (ev) => {
    ev.preventDefault();
    ev.stopPropagation();

    if (["dragenter", "dragover"].includes(ev.type)) {
      uploadForm.classList.add("--highlight");
    } else if (ev.type !== "dragleave" || ev.currentTarget === uploadForm) {
      uploadForm.classList.remove("--highlight");
    }

    if (ev.type === "drop") {
      uploader.enqueueAll(ev.dataTransfer.files);
    }
  };

  const uploadForm = form({ class: "__target" }, [
    input({
      id: "__file-input",
      type: "file",
      multiple: true,
      style: { display: "none" },
      change: (ev) => uploader.enqueueAll(ev.target.files),
    }),
    label({ for: "__file-input" }, [
      progressBar,
      p({}, [
        "Drop ",
        code({}, ".gpx"),
        ", ",
        code({}, ".tcx"),
        ", and ",
        code({}, ".fit"),
        " files here",
      ]),
    ]),
  ]);

  // lots of redundant event logic
  ["drop", "submit", "dragenter", "dragover", "dragleave"].forEach((type) =>
    uploadForm.addEventListener(type, dragHandler),
  );

  createModal(
    "Add Files",
    div({ class: "drop-area" }, [uploadForm, resultContainer]),
  );
}

class FileUploader {
  constructor(hooks) {
    this.queue = [];
    this.token = window.localStorage.getItem("api-token");
    this.onProgress = hooks.onProgress || (() => {});
    this.onComplete = hooks.onComplete || (() => {});
  }

  updateAPIToken() {
    this.token = window.prompt("Enter API token to continue uploading") || "";
    window.localStorage.setItem("api-token", this.token);
    return this.token !== "";
  }

  enqueueAll(fileList) {
    [...fileList].forEach((f) => this.enqueue(f));
  }

  enqueue(file) {
    this.queue.push(file);
    deferOnce(this, () => this._consumeQueue());
  }

  async _consumeQueue() {
    const numFiles = this.queue.length;
    let numProcessed = 0;

    while (this.queue.length !== 0) {
      const file = this.queue.pop();
      const { success, message } = await this._uploadFile(file);

      this.onProgress({
        success,
        message,
        progress: (++numProcessed / numFiles) * 100,
        fileName: file.name,
      });
    }

    this.onComplete();
  }

  static STATUS_TO_ERROR_MESSAGE = {
    400: "bad HTTP request (bug?)",
    401: "bad upload token",
    404: "uploads not enabled",
    415: "unsupported media type",
    422: "failed to read activity data",
  };

  async _uploadFile(file) {
    let res;
    try {
      const formData = new FormData();
      formData.append("file", file);
      res = await fetch("/upload", {
        method: "POST",
        body: formData,
        headers: {
          Authorization: `Bearer ${this.token}`,
        },
      });
    } catch (err) {
      // NOTE: Can be triggered by backend returning an error before consuming
      // the entire request body (as in the case of invalid file type)
      // https://github.com/hyperium/hyper/issues/2384
      console.error("Network error", err);
      return { success: false, message: err.toString() };
    }

    if (res.status === 200) {
      return { success: true, message: null };
    } else if (res.status === 401) {
      // Unauthorized -- retriable if user updates the token
      if (this.updateAPIToken()) {
        this.enqueue(file);
      } else {
        this.queue = [];
      }
    }

    return {
      success: false,
      message:
        FileUploader.STATUS_TO_ERROR_MESSAGE[res.status] ||
        "(bug) bad server response",
    };
  }
}

function createModal(header, body) {
  const { dialog, div, button } = createElement;

  const closeBtn = button({ class: "__button" }, "Close");
  const node = dialog({ class: "modal", open: "" }, [
    div({ class: "__panel" }, [
      div({ class: "__header" }, header),
      div({ class: "__body" }, body),
      div({ class: "__footer" }, [closeBtn]),
    ]),
  ]);

  // backdrop click or Escape (native <dialog>) closes; close event cleans up
  node.addEventListener("click", (ev) => {
    if (ev.target === node) node.close();
  });
  node.addEventListener("close", () => node.remove());
  closeBtn.addEventListener("click", () => node.close());

  document.body.appendChild(node);
}

class ExportButton {
  constructor(options) {
    this.options = options;
  }

  onAdd(map) {
    const { div, button } = createElement;

    // Too lazy to make a createElementNS implementation
    const btn = button({
      title: "Export image",
    });
    btn.innerHTML = `
        <svg viewBox="0 0 512 512" version="1.1" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" fill="#000000">
          <g stroke-linecap="round" stroke-linejoin="round" stroke="#CCCCCC" stroke-width="5"></g>
          <g stroke-width="0.00512" fill="none" fill-rule="evenodd">
            <g fill="#000000" transform="translate(42.666667, 42.666667)">
              <path d="M106.666667,7.10542736e-15 L106.666667,64 L362.666667,64 L362.666667,320 L426.666667,320 L426.666667,362.666667 L362.666667,362.666667 L362.666667,426.666667 L320,426.666667 L320,362.666667 L64,362.666667 L64,7.10542736e-15 L106.666667,7.10542736e-15 Z M166.336,232.64 L106.666,296.422 L106.666667,320 L320,320 L320,308.725 L274.432,263.168 L235.659405,301.959634 L166.336,232.64 Z M320,106.666667 L106.666667,106.666667 L106.666,233.982 L165.332883,171.293333 L235.648,241.621333 L274.447284,202.831976 L320,248.385 L320,106.666667 Z M245.333333,128 C263.006445,128 277.333333,142.326888 277.333333,160 C277.333333,177.673112 263.006445,192 245.333333,192 C227.660221,192 213.333333,177.673112 213.333333,160 C213.333333,142.326888 227.660221,128 245.333333,128 Z M64,64 L64,106.666667 L7.10542736e-15,106.666667 L7.10542736e-15,64 L64,64 Z"></path>
            </g>
          </g>
        </svg>
    `;

    return div(
      {
        class: "maplibregl-ctrl maplibregl-ctrl-group",
        contextmenu: (ev) => ev.preventDefault(),
        click: () => this.onClick(map),
      },
      btn,
    );
  }

  onClick(map) {
    const canvasStyle = map.getCanvas().style;
    const originalCursor = canvasStyle.cursor;

    map.dragPan.disable();
    canvasStyle.cursor = "crosshair";

    map.once("mousedown", (e) => {
      const start = e;
      const geojson = {
        type: "FeatureCollection",
        features: [
          {
            type: "Feature",
            geometry: { type: "Polygon", coordinates: [] },
          },
        ],
      };

      map.addSource("bbox", { type: "geojson", data: geojson }).addLayer({
        id: "bbox",
        source: "bbox",
        type: "fill",
        paint: {
          "fill-outline-color": "white",
          "fill-color": "#00000099",
        },
      });

      const move = (e) => {
        const bounds = new maplibregl.LngLatBounds(start.lngLat, e.lngLat);

        geojson.features[0].geometry.coordinates = [
          [
            bounds.getNorthEast().toArray(),
            bounds.getNorthWest().toArray(),
            bounds.getSouthWest().toArray(),
            bounds.getSouthEast().toArray(),
            bounds.getNorthEast().toArray(),
          ],
        ];
        map.getSource("bbox").setData(geojson);
      };

      map.on("mousemove", move).once("mouseup", (e) => {
        const aspectRatio = Math.abs(
          (start.point.y - e.point.y) / (e.point.x - start.point.x),
        );

        // west, south, east, north
        const bbox = [
          Math.min(start.lngLat.lng, e.lngLat.lng),
          Math.min(start.lngLat.lat, e.lngLat.lat),
          Math.max(start.lngLat.lng, e.lngLat.lng),
          Math.max(start.lngLat.lat, e.lngLat.lat),
        ].join(",");

        // Ensure the largest side doesn't exceed limits
        const [width, height] =
          aspectRatio <= 1
            ? [2000, Math.round(aspectRatio * 2000)]
            : [Math.round(2000 / aspectRatio), 2000];

        const qs = encodeQueryString({ bounds: bbox, width, height });
        const renderUrl = `/render?${qs}&${this.options.$queryString}`;
        window.open(renderUrl, "_blank");

        // Reset map state
        map
          .removeLayer("bbox")
          .removeSource("bbox")
          .off("mousemove", move)
          .dragPan.enable();

        canvasStyle.cursor = originalCursor;
      });
    });
  }
}

// ponytail: flat-earth approximation, fine at the ≤ 1 km nearby radius.
const metersPerDegree = (lat) => [
  111320 * Math.cos((lat * Math.PI) / 180),
  111320,
];

// [lng, lat] `meters` from `center`, `angle` radians counter-clockwise of east.
function offset({ lng, lat }, meters, angle) {
  const [mx, my] = metersPerDegree(lat);
  return [
    lng + (meters * Math.cos(angle)) / mx,
    lat + (meters * Math.sin(angle)) / my,
  ];
}

// Inverse of `offset`: [meters, angle] of `point` seen from `center`.
function polar(center, point) {
  const [mx, my] = metersPerDegree(center.lat);
  const dx = (point.lng - center.lng) * mx;
  const dy = (point.lat - center.lat) * my;
  return [Math.hypot(dx, dy), Math.atan2(dy, dx)];
}

// Approximate circle around `center` as a GeoJSON LineString.
function circle(center, meters, steps = 64) {
  const coordinates = [...Array(steps + 1).keys()].map((i) =>
    offset(center, meters, (i / steps) * 2 * Math.PI),
  );
  return { type: "LineString", coordinates };
}

// Whole meters within the panel's 10 m to 1 km (50 m if not a number).
const clampRadius = (meters) =>
  Math.min(1000, Math.max(10, Math.round(meters))) || 50;

// Click the map to list the activities passing within `options.radius` of
// that point. Hovering a row highlights its track; clicking keeps it. Drag
// the handle on the circle, or type in the panel, to change the radius.
class NearbyPanel {
  static dateFormat = new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });

  constructor(map, options) {
    const { div, span } = createElement;
    const byId = (id) => document.getElementById(id);
    this.map = map;
    this.options = options;
    this.node = byId("nearby");
    this.list = byId("nearby-list");
    this.count = byId("nearby-count");
    this.input = byId("radius");
    this.label = span();

    this.point = null;
    this.angle = 0; // of the handle while dragged, radians from east
    this.hovered = null;
    this.selected = null;
    this.tracks = new Map();
    this.requests = 0;
    this.data = { type: "FeatureCollection", features: [] };

    // Not a draggable Marker: those only track the pointer over the map
    // canvas, so they'd stall over the panel and miss a release outside it.
    const handle = div({ class: "nearby-handle" }, this.label);
    this.handle = new maplibregl.Marker({ element: handle });
    for (const type of ["mousedown", "touchstart", "touchmove", "touchend"]) {
      handle.addEventListener(type, (ev) => ev.stopPropagation()); // no panning
    }
    handle.addEventListener("pointerdown", (ev) =>
      handle.setPointerCapture(ev.pointerId),
    );
    handle.addEventListener("pointermove", (ev) => {
      if (!handle.hasPointerCapture(ev.pointerId)) return;
      const box = map.getCanvasContainer().getBoundingClientRect();
      const lngLat = map.unproject([ev.clientX - box.left, ev.clientY - box.top]);
      const [meters, angle] = polar(this.point, lngLat);
      this.angle = angle;
      this.setRadius(meters, false);
    });
    // On release, the handle jumps back to the east edge.
    handle.addEventListener("lostpointercapture", () => {
      this.angle = 0;
      this.setRadius(this.radius);
    });
    this.setRadius(+options.radius, false);

    map.on("click", (ev) => {
      if (handle.contains(ev.originalEvent.target)) return;
      this.point = ev.lngLat.wrap();
      this.selected = null;
      this.handle.setLngLat(this.edge).addTo(map);
      this.update();
    });
    options.watch(["radius", "before", "after", "filter"], () => this.update());
    this.input.addEventListener("change", () =>
      this.setRadius(this.input.valueAsNumber || this.radius),
    );
    this.list.addEventListener("mouseleave", () => this.highlight(null));
    byId("nearby-close").addEventListener("click", () => {
      this.point = null;
      this.node.hidden = true;
      this.handle.remove();
      this.draw();
    });
  }

  get edge() {
    return offset(this.point, this.radius, this.angle);
  }

  // Redraws now; `commit` also re-queries, which a drag leaves to `dragend`.
  setRadius(meters, commit = true) {
    this.radius = clampRadius(meters);
    this.input.value = this.radius;
    this.label.innerText = `${this.radius} m`;
    if (commit) this.options.radius = `${this.radius}`;
    this.draw();
  }

  // Call after (re-)adding the heatmap layer, so this stays on top.
  addLayer() {
    if (this.map.getSource("nearby")) {
      this.map
        .removeLayer("nearby")
        .removeLayer("nearby-dots")
        .removeSource("nearby");
    }

    // line-dasharray can't come from a feature property, hence two layers.
    const paint = {
      "line-color": ["get", "color"],
      "line-width": ["get", "width"],
    };
    this.map
      .addSource("nearby", { type: "geojson", data: this.data })
      .addLayer({
        id: "nearby",
        type: "line",
        source: "nearby",
        filter: ["!", ["has", "dots"]],
        paint,
      })
      .addLayer({
        id: "nearby-dots",
        type: "line",
        source: "nearby",
        filter: ["has", "dots"],
        layout: { "line-cap": "round" },
        paint: { ...paint, "line-dasharray": [0, 2] },
      });
  }

  async update() {
    if (!this.point) return;

    const { div } = createElement;
    const { lng, lat } = this.point;
    const { radius } = this;
    const { before, after, filter } = this.options;
    const qs = encodeQueryString({ lat, lng, radius, before, after, filter });

    this.node.hidden = false;
    this.count.innerText = "• • •";
    this.draw();

    const request = ++this.requests;
    const [ok, body] = await fetch(`/api/nearby?${qs}`)
      .then(async (res) => [res.ok, res.ok ? await res.json() : await res.text()])
      .catch((err) => [false, err.toString()]);

    // A newer click or setting change took over.
    if (request !== this.requests) return;

    if (!ok) {
      this.count.innerText = "Error";
      this.list.replaceChildren(div({ class: "__note" }, body));
      return;
    }

    const { total, activities } = body;
    const note =
      total === 0
        ? "Nothing passes this close."
        : total > activities.length
          ? `Newest ${activities.length} shown. Narrow the radius, dates or filter to see older ones.`
          : null;

    this.count.innerText = `${total} ${total === 1 ? "activity" : "activities"}`;
    this.list.replaceChildren(
      ...activities.map((act) => this.row(act)),
      ...(note ? [div({ class: "__note" }, note)] : []),
    );
  }

  row(act) {
    const { div, span, a } = createElement;
    const strava = act.file.match(/^strava:(\d+)$/);
    const meta = [
      act.activity_type,
      act.total_distance != null && `${act.total_distance.toFixed(1)} km`,
      act.start_time &&
        NearbyPanel.dateFormat.format(new Date(act.start_time)),
      `${Math.round(act.distance)} m away`,
    ];

    return div(
      {
        class: "__row",
        tabindex: 0,
        title: act.file,
        "data-id": act.id,
        mouseenter: () => this.highlight(act.id),
        focus: () => this.highlight(act.id),
        click: () => this.highlight(act.id, true),
      },
      [
        div({ class: "__title" }, [
          span({}, act.name || act.activity_type || act.file),
          strava
            ? a(
                {
                  href: `https://www.strava.com/activities/${strava[1]}`,
                  target: "_blank",
                  rel: "noreferrer",
                },
                "Strava ↗",
              )
            : null,
        ]),
        div({ class: "__meta" }, meta.filter(Boolean).join(" · ")),
      ],
    );
  }

  highlight(id, select = false) {
    this.hovered = id;
    if (select) {
      this.selected = id;
      this.list
        .querySelectorAll(".__row")
        .forEach((row) =>
          row.classList.toggle("--selected", +row.dataset.id === id),
        );
    }
    this.draw();
  }

  get shown() {
    return this.point && (this.hovered ?? this.selected);
  }

  async draw() {
    // Before any await, so a dragged handle snaps onto the clamped circle.
    if (this.point) this.handle.setLngLat(this.edge);

    const id = this.shown;
    if (id && !this.tracks.has(id)) {
      this.tracks.set(
        id,
        fetch(`/api/track/${id}`)
          .then((res) => res.json())
          .catch(() => null),
      );
    }
    const track = id ? await this.tracks.get(id) : null;
    if (id !== this.shown) return;

    const feature = (geometry, color, width, props) => ({
      type: "Feature",
      geometry,
      properties: { color, width, ...props },
    });
    const { point } = this;
    const spoke = () => ({
      type: "LineString",
      coordinates: [point.toArray(), this.edge],
    });

    this.data.features = [
      point && feature(circle(point, this.radius), "#6b8fff", 2),
      point && feature(spoke(), "#6b8fff", 2, { dots: true }),
      track && feature(track, "#00e5ff", 3),
    ].filter(Boolean);
    this.map.getSource("nearby")?.setData(this.data);
  }
}

class UploadButton {
  onAdd(_map) {
    const { div, button } = createElement;

    // Too lazy to make a createElementNS implementation
    const btn = button({
      title: "Add activity files",
    });
    btn.innerHTML = `
        <svg viewBox="0 0 100 100" fill="none" xmlns="http://www.w3.org/2000/svg">
            <path d="M62.75 18H49.5H31C26.5817 18 23 21.5817 23 26V74C23 78.4183 26.5817 82 31 82H68C72.4183 82 76 78.4183 76 74V50C76 50 76 50 76 50C76 50 76 40.2484 76 34" stroke="black" stroke-width="8"/>
            <path d="M62.5 18L76 34" stroke="black" stroke-width="8" stroke-linecap="round"/>
            <path d="M62 18V27.5V28C62 31.3137 64.6863 34 68 34V34H72.0625H76" stroke="black" stroke-width="8" stroke-linecap="round"/>
            <path d="M48 62C48 63.1046 48.8954 64 50 64C51.1046 64 52 63.1046 52 62H48ZM51.4142 36.5858C50.6332 35.8047 49.3668 35.8047 48.5858 36.5858L35.8579 49.3137C35.0768 50.0948 35.0768 51.3611 35.8579 52.1421C36.6389 52.9232 37.9052 52.9232 38.6863 52.1421L50 40.8284L61.3137 52.1421C62.0948 52.9232 63.3611 52.9232 64.1421 52.1421C64.9232 51.3611 64.9232 50.0948 64.1421 49.3137L51.4142 36.5858ZM52 62L52 38H48L48 62H52Z" fill="black"/>
        </svg>
    `;

    return div(
      {
        class: "maplibregl-ctrl maplibregl-ctrl-group",
        contextmenu: (ev) => ev.preventDefault(),
        click: () => createUploadModal(),
      },
      btn,
    );
  }
}

function createPropertyModal(props) {
  const { div, span } = createElement;

  const fmt = new Intl.NumberFormat();

  // null = alphabetical by key; "desc" / "asc" = by count
  let order = null;

  const body = div({ class: "__body" });
  const arrow = span({ class: "__sort-arrow" });
  const countHeader = div({ class: "__sort" }, ["Activities", arrow]);

  countHeader.addEventListener("click", () => {
    order = order === null ? "desc" : order === "desc" ? "asc" : null;
    arrow.textContent = order === "desc" ? "↓" : order === "asc" ? "↑" : "";
    renderRows();
  });

  function renderRows() {
    const sorted = Object.entries(props).sort((a, b) =>
      order === "asc"
        ? a[1].count - b[1].count
        : order === "desc"
          ? b[1].count - a[1].count
          : a[0].localeCompare(b[0]),
    );

    body.replaceChildren(
      ...sorted.map(([key, { count, types }]) =>
        div({ class: "__row" }, [
          div({ class: "__prop", title: key }, key),
          div(
            { class: "__types" },
            types.map((t) => div({ class: "__type" }, t)),
          ),
          div({ class: "__count" }, fmt.format(count)),
        ]),
      ),
    );
  }

  renderRows();

  createModal(
    "Filterable Properties",
    div({ class: "property-table" }, [
      div({ class: "__header __row" }, [
        div({}, "Key"),
        div({}, "Types"),
        countHeader,
      ]),
      body,
    ]),
  );
}

// No string escape, make sure it's trusted
function unsafeHTML(strings, ...values) {
  const htmlString = strings.reduce(
    (acc, str, i) => acc + str + (values[i] ?? ""),
    "",
  );
  const template = document.createElement("template");
  template.innerHTML = htmlString;
  return template.content;
}

function createHelpModal() {
  const { div, span, button } = createElement;

  const tabs = [
    { label: "Filters", content: filterHelpContent() },
    { label: "Gradients", content: gradientHelpContent() },
    { label: "Basemaps", content: basemapHelpContent() },
  ];

  const panels = tabs.map(({ content }) =>
    div({ class: "help-tab-panel" }, content),
  );

  const tabBtns = tabs.map(({ label }, i) => {
    const btn = button({ class: "help-tab-btn" }, label);
    btn.addEventListener("click", () => {
      tabBtns.forEach((b, j) => b.classList.toggle("--active", j === i));
      panels.forEach((p, j) => p.classList.toggle("--active", j === i));
    });
    return btn;
  });

  tabBtns[0].classList.add("--active");
  panels[0].classList.add("--active");

  createModal(
    div({ class: "help-modal-header" }, [
      span({ class: "__title" }, "Help"),
      div({ class: "help-tab-bar" }, tabBtns),
    ]),
    div({ class: "help-panels" }, panels),
  );
}

function filterHelpContent() {
  return unsafeHTML`<div class="help-content">
    <p>
      Choose which activities are included in the heatmap by providing a filter
      expression.
    </p>

    <p>
      These could be used to generate different visualizations for cycling vs
      running, exclude commutes, filter by gear, elevation, etc.
    </p>

    <p>
      Any metadata imported from your activity data can be used, as well as the
      computed properties below, which are automatically added for each
      activity.
    </p>

    <div class="__heading">Filter Syntax</div>
    <ul>
      <li>
        <code>=</code>
        <code>!=</code>
        <code>&lt;</code>
        <code>&lt;=</code>
        <code>&gt;</code>
        <code>&gt;=</code>                    — supported comparisons</li>
      <li><code>key in [a, "b c"]</code>      — match multiple string values</li>
      <li><code>key like "pattern%"</code>    — match a pattern, <code>%</code> is a wildcard</li>
      <li><code>has? "key with spaces"</code> — check if a property exists</li>
    </ul>

    <div class="__heading">Computed Properties</div>
    <ul class="__properties">
      <li><code>average_speed</code>  — average moving speed (km/h)</li>
      <li><code>elapsed_time</code>   — total activity time including pauses (seconds)</li>
      <li><code>elevation_gain</code> — total ascent (meters)</li>
      <li><code>elevation_loss</code> — total descent (meters)</li>
      <li><code>max_elevation</code>  — highest elevation (meters)</li>
      <li><code>max_speed</code>      — fastest instantaneous speed (km/h)</li>
      <li><code>min_elevation</code>  — lowest elevation (meters)</li>
      <li><code>moving_time</code>    — time spent moving (seconds)</li>
      <li><code>total_distance</code> — total distance (km)</li>
    </ul>

    <div class="__heading">Examples</div>
    <div class="__examples">
      <div class="__example">
        <code>elevation_gain > 1000</code>
        <div class="__desc">basic comparison</div>
      </div>
      <div class="__example">
        <code>"Average Temperature" < 5</code>
        <div class="__desc">keys with spaces need quotes</div>
      </div>
      <div class="__example">
        <code>activity_type in [ride, "gravel ride"]</code>
        <div class="__desc">match one of multiple values</div>
      </div>
      <div class="__example">
        <code>name like "Morning%"</code>
        <div class="__desc">wildcard pattern</div>
      </div>
      <div class="__example">
        <code>has? heart_rate</code>
        <div class="__desc">property exists</div>
      </div>
      <div class="__example">
        <code>elapsed_time < 3600 && elevation_gain > 300</code>
        <div class="__desc">combine with &&</div>
      </div>
      <div class="__example">
        <code>!(activity_type in [walk, hike])</code>
        <div class="__desc">negation</div>
      </div>
      <div class="__example">
        <code>elevation_gain > 1000 || (moving_speed > 30 && commute = true)</code>
        <div class="__desc">grouping</div>
      </div>
    </div>
  </div>`;
}

function gradientHelpContent() {
  return unsafeHTML`<div class="help-content">
    <p>
      The gradient controls the color palette used on the heatmap. It's a simple
      mapping of how many times a given tile pixel was visited at each zoom
      level to a color.
    </p>

    <p>
      This can either be one of the built-in gradients or a semicolon separated
      list of <code>threshold:color</code> stops. Pixels visited fewer than the
      first threshold are transparent; pixels visited more than the last
      threshold use the last color.
    </p>

    <p>
      Colors are given as one of <code>RGB</code>, <code>RRGGBB</code>, or
      <code>RRGGBBAA</code>. When an alpha value is not given, it's assumed to
      be fully opaque.
    </p>

    <p>
      The threshold can be as high as 255, but generally the full range won't be
      needed. At higher zoom levels, unless you ride the same route every single
      day for multiple years, generally the visit count will be relatively low.
    </p>

    <div class="__heading">Examples</div>
    <div class="__examples">
      <div class="__example">
        <code>1:FF0000;255:FFFFFF</code>
        <div class="__swatch" style="background-image: linear-gradient(in oklab to right, rgb(255 0 0), white)"></div>
      </div>
      <div class="__example">
        <code>1:000;10:fff</code>
        <div class="__swatch" style="background-image: linear-gradient(in oklab to right, black, white)"></div>
      </div>
      <div class="__example">
        <code>1:0d0221;10:7b2d8b;50:f7644a</code>
        <div class="__swatch" style="background-image: linear-gradient(in oklab to right, rgb(13 2 33), rgb(123 45 139) 18%, rgb(247 100 74))"></div>
      </div>
      <div class="__example">
        <code>1:322bb3;10:9894e5;20:fff</code>
        <div class="__swatch" style="background-image: linear-gradient(in oklab to right, rgb(50 43 179), rgb(152 148 229) 47%, white)"></div>
      </div>
      <div class="__example">
        <code>1:FF000080;10:FF0000</code>
        <div class="__swatch" style="background-image: linear-gradient(in oklab to right, rgb(255 0 0 / 50%), rgb(255 0 0))"></div>
      </div>
      <div class="__example">
        <code>1:00000077;15:6a040f;40:f00</code>
        <div class="__swatch" style="background-image: linear-gradient(in oklab to right, rgb(0 0 0 / 47%), rgb(106 4 15) 36%, rgb(255 0 0))"></div>
      </div>
    </div>
  </div>`;
}

function basemapHelpContent() {
  return unsafeHTML`<div class="help-content">
    <p>
      In addition to the minimal built-in basemaps, custom raster tile layers
      can be used as the background layer. First set the basemap to <code>Custom
      Raster</code>, then paste in the tile URL, which will look something like
      <code>https://example.com/{z}/{x}/{y}.png</code>.
    </p>

    <p>
      <a href="https://leaflet-extras.github.io/leaflet-providers/preview/">Leaflet-providers</a>
      has a large list of (mostly) freely available tile layers, although some
      may require an API key.
    </p>

    <p>
      This is also a tile server, which means you can render two separate
      heatmaps on top of each other. For example, set the date filters to select
      last year's activities with one gradient, copy the formatted tile URL as
      the basemap, then set the date filters to select this year's activities
      with a different gradient.
    </p>
  </div>`;
}
