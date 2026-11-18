package org.pengudroid.composeactivity

import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

private const val TAG = "PenguDroidCompose"

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Log.i(TAG, "Compose MainActivity onCreate")
        setContent {
            ComposeFixtureScreen()
        }
    }

    override fun onResume() {
        super.onResume()
        Log.i(TAG, "Compose MainActivity onResume")
    }
}

@Composable
fun ComposeFixtureScreen() {
    var enteredText by remember { mutableStateOf("") }
    var clicks by remember { mutableIntStateOf(0) }

    Surface(modifier = Modifier.fillMaxSize(), color = Color(0xFF183048)) {
        Column(
            modifier = Modifier.fillMaxSize().padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            Text("PenguDroid: Jetpack Compose", color = Color.White)
            OutlinedTextField(
                value = enteredText,
                onValueChange = {
                    enteredText = it
                    Log.i(TAG, "Compose text changed: $it")
                },
                label = { Text("Поле ввода") },
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = {
                    clicks++
                    Log.i(TAG, "Compose button clicked: $clicks")
                },
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text("Кнопка: $clicks")
            }
            Text("Введено: $enteredText", color = Color.White)
        }
    }
}
