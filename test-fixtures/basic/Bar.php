<?php

declare(strict_types=1);

namespace App;

use App\Test\Baz;

class Bar
{
    public function greet(
        string $name,
        Baz $baz2
    ): string
    {
        (new Foo())->increment(5);

        helper();

        $baz = new Baz();
        $baz->test();

        $baz2->test();

        echo $baz2->test;

        return "Hello, {$name}!";
    }
}

function helper(string $s1, string $s2): void
{
    echo "This is a helper function.";
}
