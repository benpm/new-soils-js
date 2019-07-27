"use strict";

/*global $, window, document*/

/**
 * Properties for a generic UIElement
 * @typedef {*} UIElementProperties
 * @property {string} [text] Text added to element (simply adds a paragraph tag, may not work for all types of UI elements)
 * @property {boolean} [hidden] Visibility of element
 */

/**
 * Properties for a panel
 * @typedef {UIElementProperties} Panel~props
 * @prop {number} [width] Width of panel
 * @prop {number} [height] Height of panel
 * @prop {boolean} [resizable] Whether panel can be resized
 * @prop {boolean} [moveable] Whether panel can be dragged
 * @prop {string} [label] Text label to add to panel
 * @prop {number} [x] Horizontal position of panel in window
 * @prop {number} [y] Vertical position of panel in window
 * @prop {boolean} [center] Whether to keep panel centered in window
 */

/**
 * Properties for an text input element
 * @typedef {UIElementProperties} Input~props
 * @prop {string} name What to label the input with
 * @prop {string} [type] Type of text input ("password", for example)
 */

/**
 * Properties for a button
 * @typedef {UIElementProperties} Button~props
 * @prop {string} name What to label the button with
 * @prop {function(event:JQueryEventObject)} [action] Click handler
 * @prop {boolean} [stacked=false] Whether button is fullwidth and stacked
 */

/**
 * Properties for a slider element
 * @typedef {UIElementProperties} Slider~props
 * @prop {string} [name] What to label the slider with
 * @prop {function(value:number, element:Slider)} [action] Slide handler
 * @prop {boolean} [stacked=false] Whether button is fullwidth and stacked
 * @prop {Symbol} [display=percent] Display style of slider value
 * @prop {number} [value] Default and initial value of slider
 * @prop {number[]} [bounds=[0, 100]] Low and high bounds of slider value
 */

/**
 * Properties for a checkbox element
 * @typedef {UIElementProperties} Checkbox~props
 * @prop {string} [name] What to label the slider with
 * @prop {function(value:number, element:Slider)} [action] Slide handler
 * @prop {boolean} [stacked=false] Whether button is fullwidth and stacked
 * @prop {Symbol} [display=percent] Display style of slider value
 * @prop {number} [value] Default and initial value of slider
 * @prop {number[]} [bounds=[0, 100]] Low and high bounds of slider value
 */

var ui = (function ui() {
	//Setup
	$("body").append($("<div id='ui'>"));
	var uiParent = $("#ui");
	var scale = 2;
	var loadingImg = $("<img>", {src: "img/loading-gear.png", class:"loading"});

	//Symbols
	var percent = Symbol();
	var wholeNumber = Symbol();

	//For .centered objects
	$(window).resize(function(){
		let centered = $(".center");
		centered.css("left", Math.floor((
			$(window).width() - parseInt(centered.css("width"))) / 2));
		centered.css("top", Math.floor((
			$(window).height() - parseInt(centered.css("height"))) / 2));
	});

	/** The base UIElement class. All others extend this class. */
	class UIElement {
		/**
		 * @param  {string} name
		 * @param  {UIElementProperties} props
		 * @param  {Tabs|Panel} parent
		 */
		constructor(name, props, parent) {
			//Hidden props
			this._hidden = false;
			this._text = null;

			//Setup
			this.name = name;
			this.parent = parent;
			this.parentHTML = parent ? parent.html : uiParent;
			this.html = $("<div>", {class: name});
			this.parentHTML.append(this.html);
			this.hidden = props.hidden || false;
			if (props.text) this.text = props.text;
		}
		get hidden() {
			return this._hidden;
		}
		set hidden(val) {
			this._hidden = val;
			this.html.css("display", val ? "none" : "");
		}
		get text() {
			return this._text ? this._text.text() : "";
		}
		set text(val) {
			if (!this._text) this.html.append(this._text = $("<p>"));
			this._text.html(val);
		}
	}

	/** A panel containing other UI elements */
	class Panel extends UIElement {
		/**
		 * @param  {Panel~props} props
		 * @param  {Tabs} [parent]
		 * @param  {string} [type]
		 */
		constructor(props, parent, type) {
			super(type ? type : "panel", props ? props : {}, parent);

			//Panel size
			this.width = props.width ? props.width : 150;
			this.height = props.height ? props.height : 150;

			//Resizable / Moveable
			if (props.resizable) this.html.resizable();
			if (props.moveable) {
				this.html.addClass("moveable");
				if (parent) parent.html.addClass("moveable");
				this.html.draggable({
					containment: "window",
					cancel: ".slide, .checkbox"
				});
			}

			//Label
			this.label = props.label ? new Label({
				text: props.label
			}, this) : null;

			//Position
			if (props.x) this.x = props.x;
			if (props.y) this.y = props.y;
			if (props.center) {
				this.x = Math.floor(($(window).width() - this.width * scale) / 2);
				this.y = Math.floor(($(window).height() - this.height * scale) / 2);
				this.center = true;
				this.html.addClass("center");
			}

			//Tabs
			if (this.parent && this.parent.name == "tabs") {
				this.x = 0;
				this.y = 0;
				this.tab = parent.tabulate(this, props);
				this.parent.current = this;
			}

			//Other
			this._loading = false;
			this.html.mousedown(Panel.focus);
			if (this.name != "tabs") this.html.addClass("wooden");
			this.dom();
		}
		dom() {
			if (this.name == "tabs" && this.center) {
				this.x = Math.floor(($(window).width() - this.width * scale) / 2);
				this.y = Math.floor(($(window).height() - this.height * scale) / 2);
			}
			this.html.css("width", this.width * scale);
			this.html.css("height", this.height * scale);
			this.html.css("left", this.x);
			this.html.css("top", this.y);
		}

		//Clears input from all children
		clear() {
			$(this.html).find("input").val("");
		}

		//Hides children, shows spinning gear
		get loading() {
			return this._loading;
		}
		set loading(val) {
			this._loading = val;
			if (val) {
				this.html.children().hide();
				this.html.append(loadingImg);
				if (this.name == "tabs") {
					this.locked = true;
				}
			} else {
				this.html.children().show();
				this.html.find(".loading").detach();
				if (this.name == "tabs") {
					this.current = this.current;
					this.locked = false;
				}
			}
		}

		//Focusing handler
		static focus() {
			$("#ui .panel, #ui .tabs").css("z-index", "2");
			$(this).css("z-index", "4");
		}
	}

	/** A panel that contains panels */
	class Tabs extends Panel {
		/**
		 * @param  {Panel~props} props
		 */
		constructor(props) {
			super(props, null, "tabs");
			this._current = null;
			this.locked = false;
			this.bar = $("<div>", {class: "tab-bar"});
			this.html.prepend(this.bar);
			this.tabs = 0;
			this.width = 0;
			this.height = 0;
			this.dom();
		}

		//Update tabs (called by child Panels)
		tabulate(child, props) {
			//Tab elements
			this.tabs++;
			let tab = $("<div>", {class: "tab"});
			this.bar.append(tab);
			tab.text(props.label);
			child.label.html.detach();
			tab.click(function(){
				if ($(this).attr("state") == "unfocus" && !child.parent.locked)
					child.parent.current = child;
			});

			//Adjust size / pos
			if (child.height > this.height) this.height = child.height + 10;
			if (child.width > this.width) this.width = child.width + 8;
			$(this.html).find(".tab").css("width", `${100 / this.tabs}%`);
			$(this.html).find(".tab").addClass("wooden");

			//Update and return reference to child
			this.dom();
			child.dom();
			return tab;
		}

		//Select current open tab
		get current() {
			return this._current;
		}
		set current(val) {
			this._current = val;
			$(this.html).find(".panel").hide();
			$(val.html).show();
			$(this.html).find(".tab").attr("state", "unfocus");
			$(val.tab).attr("state", "focus");
			val.clear();
		}
	}

	/** A useful text label */
	class Label extends UIElement {
		constructor(props, parent) {
			super("label", props || {}, parent);
			this.dom();
		}
		dom() {
		}
	}

	/** A text field */
	class Input extends UIElement {
		/**
		 * @param  {Input~props} props
		 * @param  {Panel} parent
		 */
		constructor(props, parent) {
			super("input", props || {}, parent);
			this.html.append(this.form = $("<form>", {action: "javascript:void(0);"}));
			this.form.append(this.input = $("<input>", {
				type: "text", 
				name: props.name,
				placeholder: props.name
			}));
			if (props.type) this.input.attr("type", props.type);
			this.dom();
		}
		dom() {

		}
	}

	/** A pressable, actionable button */
	class Button extends UIElement {
		/**
		 * @param  {Button~props} props
		 * @param  {Panel} parent
		 */
		constructor(props, parent) {
			super("button", props ? props : {}, parent);
			this.html.append(this.button = $("<button>"));
			this.button.text(props.name);
			if (props.action) this.html.click({element: this}, props.action);
			if (props.stacked) this.html.addClass("stacked");
			this.button.addClass("stone");
			this.dom();
		}
		dom() {

		}
	}

	/** Formatted text */
	class Text extends UIElement {
		/**
		 * @param  {UIElementProperties} props
		 * @param  {Panel} parent
		 */
		constructor(props, parent) {
			super("text", props ? props : {}, parent);
			this.dom();
		}
		dom() {

		}
	}

	/** A customizable slider */
	class Slider extends UIElement {
		/**
		 * @param  {Slider~props} props
		 * @param  {Panel} parent
		 */
		constructor(props, parent) {
			super("slider", props ? props : {}, parent);

			//Style of slider
			this.label = props.name || "Slider";
			this.displayStyle = props.display || percent;

			//Display method setup
			this.default = props.value || 0;
			this.lower = props.bounds ? props.bounds[0] : 0;
			this.upper = props.bounds ? props.bounds[1] - this.lower : 100;
			this.amount = 0;
			this.value = 0;
			this.max = parent.width * 2 - 14;
			this.amount = ((this.default - this.lower) / this.upper) * this.max;

			//Action
			this.clicked = false;
			this.action = props.action || null;

			//DOM setup
			this.lastx = 0;
			this.html.append(this.slide = $("<div class='slide'>"));
			this.slide.mousedown({slider: this}, this.ondown);
			this.slide.addClass("stone");
			$(document).mouseup({slider: this}, this.onup);
			$(document).mousemove({slider: this}, this.onmove);
			if (props.stacked) this.html.addClass("stacked");
			this.dom();
		}
		ondown(event) {
			//Initiate slide action
			event.data.slider.clicked = true;
			event.data.slider.lastx = event.clientX;
		}
		onmove(event) {
			//Get slider element
			let slider = event.data.slider;
			let dx = event.clientX - slider.lastx;
			if (!slider.clicked) return;

			//Slide, minding boundaries
			if (slider.amount + dx < 0)
				slider.amount = 0;
			else if (slider.amount + dx > slider.max)
				slider.amount = slider.max;
			else {
				slider.amount += dx;
				slider.lastx = event.clientX;
			}

			//Update DOM
			slider.dom();
		}
		onup(event) {
			//End slide action
			event.data.slider.clicked = false;
		}
		dom() {
			//Show position of slider
			let previous = this.value;
			this.value = this.lower + Math.round((this.amount / this.max) * this.upper);
			this.slide.css("margin-left", `${this.amount}px`);

			//Display text styles
			switch(this.displayStyle) {
				case percent:
					this.text = `${this.label}: ${this.value}%`;
					break;
				case wholeNumber:
					this.text = `${this.label}: ${this.value}`;
					break;
			}

			//Call action
			if (this.action && this.value != previous) this.action(this.value, this);
		}
	}

	/** A toggle switch, actionable */
	class Checkbox extends UIElement {
		/**
		 * @param  {Checkbox~props} props
		 * @param  {Panel} parent
		 */
		constructor(props, parent) {
			super("checkbox", props ? props : {}, parent);
			this.html.append(this.checkbox = $("<input type='checkbox'>"));
			if (props.checked) this.checkbox.click();
			if (props.action) this.html.click({element: this}, props.action);
			if (props.stacked) this.html.addClass("stacked");
			this.text = props.name || "Checkbox";
			this.dom();
		}
		dom() {

		}
	}

	return {
		Panel, Input, Button, Tabs, Text, Slider, Checkbox,

		/** Slider: specifies percentage display */
		percent,

		/** Slider: specifies whole number display */
		wholeNumber
	};
})();
