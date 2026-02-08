<?php

// Intentionally broken PHP for testing error recovery

class Foo {
    public function bar( {
        // Missing closing paren
    }

    public function baz(): void
    {
        $x = ;  // Missing expression
    }
}

function incomplete(

class MissingBrace {
    public $prop;
