"use strict";

var colorThief = new ColorThief();
var clrChangeA = [0, 0, 0];
var clrChangeB = [0, 0, 0];
var clrGoalA = [10, 10, 10];
var clrGoalB = [0, 0, 0];

String.prototype.format = function() {
    var i = -1;
    var arg = arguments;
    return this.replace(/(%s)/g, function(match) {
        i += 1;
        return typeof arg[i] != 'undefined' ? arg[i] : "???";
    });
};

String.prototype.nformat = function() {
    var arg = arguments;
    return this.replace(/{(\d+)}/g, function(match, i) {
        return typeof arg[i] != 'undefined' ? arg[i] : match;
    });
};

function imgTiles(root) {
    $.get(root + "/info.txt", function(data){
        
        //Split data into list
        var IdList = data.split("\n");
        
        //Iterate thru data
        for (var listItem of IdList) {
            
            //Split into two parts (ID and TITLE)
            var Id = listItem.split("|")[0];
            var title = listItem.split("|")[1];
            var link = listItem.split("|")[2];
            
            //Create tiles
            if (title)
            switch (root) {
                case "img/videos":
                    $(".container").append(`<div class='item'>
                        <a target='_blank' href='https://youtu.be/{0}'>
                        <img data-txt='{1}' src='{2}/{0}.jpg' /></a></div>`.nformat(
                            Id, title, root));
                    break;
                case "img/music":
                    $(".container").append(`<div class='item'>
                        <a target='_blank' href='{1}'>
                        <img data-txt='{3}' src='{2}/{0}.jpg' /></a></div>`.nformat(
                            Id, title, root, titleize(title.split("/").pop())));
                    break;
                case "img/photography":
                    $(".container").append(`<div class='item'>
                        <a target='_blank' href='{2}/{0}.jpg'>
                        <img data-txt='{1}' src='{2}/{0}.jpg' /></a></div>`.nformat(
                            Id, title, root));
                    break;
                case "img/gamedev":
                    $(".container").append(`<div class='item bar' 
                        style="background: url('{2}/{1}.jpg') center; background-size: cover;">
                        <h2>{3}</h2><p>{0}</p></div>`.nformat(
                            Id, title, root, titleize(title)));
                    break;
                case "img/experiments":
                    var color = "background: linear-gradient(%sdeg, hsl(0,0%,%s%), hsl(0,0%,%s%));".format(
                        Math.floor(Math.random() * 360),
                        Math.floor(Math.random() * 20),
                        Math.floor(Math.random() * 20));
                    $(".container").append(`<a target='_blank' href="{2}">
                        <div class='item sml' 
                        style="{3}">
                        <h2>{1}</h2><p>{0}</p></div></a>`.nformat(
                            Id, title, link, color));
                    break;
            }
        }
        postLoad();
    });
}

function titleize(string) {
    string = string.replace(/[!@#$%^&*()_+-=]/g, " ");
    string = string.replace(/\B[A-z]/g, 
                   function(match){return match.toLowerCase();});
    string = string.replace(/\b[A-z]/g, 
                   function(match){return match.toUpperCase();});
    return string;
}

function transColor(color, scalar) {
    color[0] = Math.min(Math.floor(color[0] * scalar), 255);
    color[1] = Math.min(Math.floor(color[1] * scalar), 255);
    color[2] = Math.min(Math.floor(color[2] * scalar), 255);
}

function lerpColor(colorA, colorB, speed) {
    colorA[0] = Math.min((colorA[0] + (colorB[0] - colorA[0]) * speed * 0.5), 255);
    colorA[1] = Math.min((colorA[1] + (colorB[1] - colorA[1]) * speed * 0.5), 255);
    colorA[2] = Math.min((colorA[2] + (colorB[2] - colorA[2]) * speed * 0.5), 255);
}

function assignColor(element, color) {
    element.css("background", 
            "rgb(%s,%s,%s)".format(
            Math.floor(color[0]),
            Math.floor(color[1]),
            Math.floor(color[2])));
}

function assignGradient(element, colorA, colorB) {
    element.css("background", 
            "linear-gradient(to bottom, rgb(%s,%s,%s), rgb(%s,%s,%s))".format(
            Math.floor(colorA[0]),
            Math.floor(colorA[1]),
            Math.floor(colorA[2]),
            Math.floor(colorB[0]),
            Math.floor(colorB[1]),
            Math.floor(colorB[2])));
}

function postLoad() {
    console.log("LOADED");
    $(".item img").mouseenter(function(){
        
        //Set alt text
        $(".header p").text($(this).data("txt"));
        
        //Grab and transform colors
        var color = colorThief.getPalette(this, 4);
        transColor(color[0], 0.5);
        transColor(color[1], 0.5);
        transColor(color[2], 0.5);
        transColor(color[3], 0.5);
        
        //Set colors
        clrGoalA = color[1];
        clrGoalB = color[2];
    });
    $(".bar").mouseenter(function(){
        
        //Set alt text
        $(".header p").text($(this).children("h2").text());
        
        //Grab and transform colors
        var bg = $(this).css("background");
        var img = document.createElement("img");
        img.src = bg.slice(bg.search(/\("/g) + 2, bg.search(/"\)/g));
        var color = colorThief.getPalette(img, 4);
        console.log(color);
        transColor(color[0], 0.5);
        transColor(color[1], 0.5);
        transColor(color[2], 0.5);
        transColor(color[3], 0.5);

        //Set colors
        clrGoalA = color[1];
        clrGoalB = color[2];
    });
    setInterval(function(){
        lerpColor(clrChangeA, clrGoalA, 0.05);
        lerpColor(clrChangeB, clrGoalB, 0.05);
        assignGradient($(".header"), clrChangeA, clrChangeB);
        $("html").css("background", $(".header").css("background"));
    }, 33);
}

